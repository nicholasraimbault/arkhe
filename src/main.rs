//! arkhd — the arkhe supervisor
//!
//! Architecture: ECS (Entity-Component-System) pattern.
//! - World struct holds all service state as parallel arrays
//! - Systems are plain functions dispatched from the io_uring event loop
//! - The io_uring ring IS the ECS scheduler
//!
//! See docs/ARCHITECTURE.md for the full design.

mod cgroup;
mod components;
mod config;
mod error;
mod ring;
pub mod sandbox;
mod sys;
mod world;

pub mod systems {
    pub mod deps;
    pub mod log;
    pub mod mounts;
    pub mod pressure;
    pub mod socket;
    pub mod spawn;
    pub mod status;
    pub mod supervise;
}

use std::os::fd::OwnedFd;
use std::path::Path;

use io_uring::IoUring;

use components::RuntimeState;
use error::SupervisorError;
use ring::{build_poll_multishot, decode_tag, Tag};
use world::World;

/// Log a message to stderr with the arkhd prefix.
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("arkhd: {}", format_args!($($arg)*))
    };
}

fn main() {
    log!("starting");

    if let Err(e) = run() {
        log!("fatal: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), SupervisorError> {
    // 1. Set up signalfd for SIGCHLD, SIGTERM, SIGHUP
    let signal_fd = sys::setup_signals()?;
    log!("signals: signalfd ready");

    // 2. Create io_uring ring
    let mut ring = IoUring::new(64).map_err(SupervisorError::RingInit)?;
    log!("ring: io_uring ready");

    // 3. Create runtime directories
    create_dir("/run/arkhe")?;
    create_dir("/run/ready")?;
    create_dir("/var/log/arkhe")?;

    // 4. Scan /etc/sv/ and populate World
    let mut world = World::new();
    world.landlock_abi = sys::landlock_abi_version();
    log!("landlock: ABI version {}", world.landlock_abi);
    scan_services(&mut world);
    log!("config: {} services loaded", world.len());

    // 5. Scan /run/ready/ for pre-existing readiness files
    systems::deps::scan_initial_readiness(&mut world);

    // 6. Set up dependency watcher — always use polling (reliable on all filesystems).
    //    fanotify with FAN_REPORT_DFID_NAME doesn't fire events on tmpfs (/run/ready/),
    //    and the ONN tablet kernel (5.15) has CONFIG_FANOTIFY=n anyway.
    //    Polling at 500ms adds ~2 syscalls per interval — negligible overhead.
    // Dependency watcher: always use polling (fanotify doesn't work on tmpfs).
    // The event loop uses submit_and_wait with a 1-second timeout and calls
    // on_deps_poll() every iteration.
    log!("deps: polling mode (1s interval)");
    // Keep fanotify fd slot for future use if needed
    let fanotify_fd: Option<std::os::fd::OwnedFd> = None;

    // 7. Write supervisor PID file for CLI communication
    let _ = std::fs::write("/run/arkhe/arkhd.pid", std::process::id().to_string());

    // 8. Bind sockets for socket-activated services
    if let Err(e) = systems::socket::setup_sockets(&mut world, &mut ring) {
        log!("socket: setup failed: {e}");
    }

    // 8. Submit signalfd poll to the ring
    let signal_sqe = build_poll_multishot(&signal_fd, Tag::Signal);
    sys::push_sqe(&mut ring, &signal_sqe)?;
    ring.submit().map_err(SupervisorError::RingSubmit)?;

    // 9. Spawn services with satisfied dependencies
    if let Err(e) = systems::deps::spawn_ready_services(&mut world, &mut ring) {
        log!("deps: initial spawn failed: {e}");
    }
    ring.submit().map_err(SupervisorError::RingSubmit)?;

    // 9. Event loop
    let mut shutting_down = false;
    event_loop(
        &mut ring,
        &signal_fd,
        fanotify_fd.as_ref(),
        &mut world,
        &mut shutting_down,
    )?;

    log!("shutdown complete");
    Ok(())
}

/// Main event loop — submit and wait, then dispatch completions by tag.
fn event_loop(
    ring: &mut IoUring,
    signal_fd: &OwnedFd,
    fanotify_fd: Option<&OwnedFd>,
    world: &mut World,
    shutting_down: &mut bool,
) -> Result<(), SupervisorError> {
    // Create a timerfd for 1-second periodic deps polling.
    let timer_fd = sys::create_timerfd(1)?;
    let timer_sqe = build_poll_multishot(&timer_fd, Tag::DepsPoll);
    sys::push_sqe(ring, &timer_sqe)?;

    let mut last_deps_poll = std::time::Instant::now();

    loop {
        // Non-blocking submit + CQE reap. We avoid submit_and_wait(1) because
        // io_uring multishot poll on timerfd is unreliable on some kernels,
        // which would cause the loop to block indefinitely with no CQEs.
        // Instead, submit pending SQEs, collect available CQEs, and sleep
        // briefly when idle. This guarantees deps polling runs every second.
        ring.submit().map_err(SupervisorError::RingSubmit)?;

        let tags: Vec<Tag> = ring
            .completion()
            .map(|cqe| decode_tag(cqe.user_data()))
            .collect();

        if tags.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        for tag in tags {
            match tag {
                Tag::Signal => {
                    handle_signals(signal_fd, world, ring, shutting_down)?;
                }
                Tag::Pidfd(id) => {
                    if id < world.len() {
                        if let Err(e) =
                            systems::supervise::on_service_exit(world, id, ring, *shutting_down)
                        {
                            log!(
                                "supervise: error handling exit for {}: {e}",
                                world.names[id]
                            );
                        }
                        if let Err(e) = systems::deps::spawn_ready_services(world, ring) {
                            log!("deps: error after exit: {e}");
                        }
                    }
                }
                Tag::Inotify => {
                    if let Some(fan_fd) = fanotify_fd {
                        if let Err(e) = systems::deps::on_fanotify_event(world, fan_fd, ring) {
                            log!("deps: event processing error: {e}");
                        }
                    }
                }
                Tag::Splice(id) => {
                    if id < world.len() {
                        if let Err(e) = systems::log::on_log_readable(world, id) {
                            log!("log: error for {}: {e}", world.names[id]);
                        }
                    }
                }
                Tag::Restart(id) => {
                    if id < world.len() {
                        if let Err(e) = systems::supervise::on_restart_timeout(world, id, ring) {
                            log!("supervise: restart error for {}: {e}", world.names[id]);
                        }
                    }
                }
                Tag::StopTimeout(id) => {
                    if id < world.len() {
                        if let Err(e) = systems::supervise::on_stop_timeout(world, id) {
                            log!("supervise: stop timeout error for {}: {e}", world.names[id]);
                        }
                    }
                }
                Tag::Accept(id) => {
                    if id < world.len() {
                        if let Err(e) = systems::socket::on_accept(world, id, ring) {
                            log!("socket: accept error for {}: {e}", world.names[id]);
                        }
                    }
                }
                Tag::Psi(id) => {
                    if id < world.len() {
                        systems::pressure::on_pressure(world, id);
                    }
                }
                Tag::DepsPoll => {
                    sys::read_timerfd(&timer_fd);
                    if !*shutting_down {
                        if let Err(e) = systems::deps::on_deps_poll(world, ring) {
                            log!("deps: poll error: {e}");
                        }
                        last_deps_poll = std::time::Instant::now();
                    }
                }
            }
        }

        // Rate-limited deps polling — runs on every CQE batch as a fallback
        // in case the timerfd multishot poll doesn't fire reliably.
        if last_deps_poll.elapsed().as_secs() >= 1 && !*shutting_down {
            if let Err(e) = systems::deps::on_deps_poll(world, ring) {
                log!("deps: poll error: {e}");
            }
            last_deps_poll = std::time::Instant::now();
        }

        if *shutting_down {
            // Kill all running services before exiting
            for id in 0..world.len() {
                match world.states[id] {
                    RuntimeState::Running { pid, .. }
                    | RuntimeState::Ready { pid, .. }
                    | RuntimeState::Stopping { pid, .. } => {
                        let _ = sys::kill_service(pid, libc::SIGKILL);
                    }
                    _ => {}
                }
                world.pidfds[id] = None;
            }
            break;
        }
    }

    Ok(())
}

/// Drain all pending signals from the signalfd.
fn handle_signals(
    signal_fd: &OwnedFd,
    world: &mut World,
    ring: &mut IoUring,
    shutting_down: &mut bool,
) -> Result<(), SupervisorError> {
    loop {
        match sys::read_signal(signal_fd)? {
            Some(info) => match info.signo {
                libc::SIGCHLD => {
                    log!("signal: SIGCHLD from pid {}", info.pid);
                }
                libc::SIGTERM => {
                    log!("signal: SIGTERM, shutting down");
                    *shutting_down = true;
                }
                libc::SIGHUP => {
                    log!("signal: SIGHUP — processing control files and hot-reloading services");
                    if let Err(e) = systems::supervise::process_control_files(world, ring) {
                        log!("ctl: error processing control files: {e}");
                    }
                    // Immediately re-scan /etc/sv/ so that `ark reload` takes effect
                    // without waiting for the next polling interval.
                    if let Err(e) = systems::deps::on_deps_poll(world, ring) {
                        log!("reload: error during hot-reload scan: {e}");
                    }
                }
                _ => {
                    log!("signal: unexpected signo {}", info.signo);
                }
            },
            None => break,
        }
    }
    Ok(())
}

/// Scan /etc/sv/ for service directories and load each one.
fn scan_services(world: &mut World) {
    let sv_dir = Path::new("/etc/sv");
    let entries = match std::fs::read_dir(sv_dir) {
        Ok(entries) => entries,
        Err(e) => {
            log!("config: cannot read /etc/sv: {e}");
            return;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                log!("config: readdir error: {e}");
                continue;
            }
        };

        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        match config::load_service(world, &path) {
            Ok(name) => log!("config: loaded {name}"),
            Err(e) => log!("config: skipping {}: {e}", path.display()),
        }
    }
}

/// No-op signal handler for SIGALRM (used to interrupt io_uring submit_and_wait).
extern "C" fn noop_signal_handler(_: libc::c_int) {}

/// Create a directory, ignoring "already exists" errors.
fn create_dir(path: &str) -> Result<(), SupervisorError> {
    match std::fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(SupervisorError::DirCreate(e)),
    }
}
