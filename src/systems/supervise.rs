//! Supervision system — handle process exits, restart with backoff, stop/start control.
//!
//! When Tag::Pidfd(id) fires, the service's process has exited.
//! This system reads the exit status, cleans up, and schedules a restart
//! with exponential backoff via io_uring timeout (Tag::Restart(id)).
//!
//! Stop/start control is triggered by SIGHUP: the CLI writes control files
//! to /run/arkhe/<name>/ctl/ and sends SIGHUP. The supervisor scans these
//! files and acts accordingly.
//!
//! Zero unsafe in this file.

use std::path::Path;
use std::time::{Duration, Instant};

use io_uring::IoUring;

use crate::components::RuntimeState;
use crate::error::SupervisorError;
use crate::ring::{build_timeout, Tag};
use crate::sys;
use crate::world::{ServiceId, World};

/// Handle a service process exit. Called when Tag::Pidfd(id) fires.
pub fn on_service_exit(
    world: &mut World,
    id: ServiceId,
    ring: &mut IoUring,
    shutting_down: bool,
) -> Result<(), SupervisorError> {
    let name = world.names[id].clone();
    let old_state = world.states[id];

    // 1. Read exit status via pidfd
    let (exit_code, signal) = match &world.pidfds[id] {
        Some(pidfd) => sys::waitid_pidfd(pidfd).unwrap_or((None, None)),
        None => (None, None),
    };

    // 2. Update state
    world.states[id] = RuntimeState::Stopped { exit_code, signal };

    // 3. Clean up — RAII closes pidfd
    world.pidfds[id] = None;

    // 4. Write state files
    write_state_files(&name, 0, "stopped");

    // 5. If this was a manual stop (Stopping state), don't restart
    if matches!(old_state, RuntimeState::Stopping { .. }) {
        eprintln!(
            "arkhd: supervise: {name} stopped (code={exit_code:?} signal={signal:?})"
        );
        return Ok(());
    }

    // 6. Restart logic — only if enabled and not shutting down
    if !shutting_down && world.run_configs[id].enabled {
        let rs = &mut world.restart_states[id];
        rs.count += 1;
        rs.backoff_secs = backoff_secs(rs.count);
        rs.last_restart = Instant::now();

        if rs.backoff_secs == 0 {
            world.states[id] = RuntimeState::NotStarted;
            write_state_files(&name, 0, "starting");
        } else {
            let (ts_ptr, handle) = sys::alloc_timespec(rs.backoff_secs);
            world.restart_timeout_ptrs[id] = handle;
            let sqe = build_timeout(ts_ptr, Tag::Restart(id));
            sys::push_sqe(ring, &sqe)?;

            world.states[id] = RuntimeState::Failing {
                last_exit: exit_code,
                restart_count: rs.count,
                next_restart: Instant::now() + Duration::from_secs(rs.backoff_secs),
            };
            write_state_files(&name, 0, "failing");
        }

        eprintln!(
            "arkhd: supervise: {name} exited (code={exit_code:?} signal={signal:?}), \
             restart in {}s (attempt {})",
            rs.backoff_secs, rs.count
        );
    } else {
        eprintln!(
            "arkhd: supervise: {name} exited (code={exit_code:?} signal={signal:?}), not restarting"
        );
    }

    Ok(())
}

/// Handle a restart timeout expiry. Called when Tag::Restart(id) fires.
pub fn on_restart_timeout(
    world: &mut World,
    id: ServiceId,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    sys::free_timespec(world.restart_timeout_ptrs[id]);
    world.restart_timeout_ptrs[id] = 0;

    let name = world.names[id].clone();
    eprintln!("arkhd: supervise: restart timer fired for {name}");

    world.states[id] = RuntimeState::NotStarted;
    world.log_pipe_fds[id] = None;
    world.log_file_fds[id] = None;

    write_state_files(&name, 0, "starting");
    crate::systems::deps::spawn_ready_services(world, ring)?;

    Ok(())
}

/// Stop a running service: SIGTERM now, SIGKILL after 5s grace period.
pub fn stop_service(
    world: &mut World,
    id: ServiceId,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    let name = world.names[id].clone();

    let pid = match world.states[id] {
        RuntimeState::Running { pid, .. } | RuntimeState::Ready { pid, .. } => pid,
        _ => {
            eprintln!("arkhd: supervise: {name} not running, nothing to stop");
            return Ok(());
        }
    };

    // Disable auto-restart
    world.run_configs[id].enabled = false;

    // Send SIGTERM
    if let Err(e) = sys::kill_service(pid, libc::SIGTERM) {
        eprintln!("arkhd: supervise: SIGTERM to {name}: {e}");
    }

    // Schedule SIGKILL after 5s grace period
    let (ts_ptr, handle) = sys::alloc_timespec(5);
    world.restart_timeout_ptrs[id] = handle;
    let sqe = build_timeout(ts_ptr, Tag::StopTimeout(id));
    sys::push_sqe(ring, &sqe)?;

    world.states[id] = RuntimeState::Stopping {
        pid,
        stop_requested_at: Instant::now(),
    };
    write_state_files(&name, pid, "stopping");
    eprintln!("arkhd: supervise: stopping {name} (pid {pid})");

    Ok(())
}

/// Handle stop timeout — send SIGKILL if service is still running.
pub fn on_stop_timeout(world: &mut World, id: ServiceId) -> Result<(), SupervisorError> {
    sys::free_timespec(world.restart_timeout_ptrs[id]);
    world.restart_timeout_ptrs[id] = 0;

    if let RuntimeState::Stopping { pid, .. } = world.states[id] {
        let name = &world.names[id];
        eprintln!("arkhd: supervise: {name} grace period expired, sending SIGKILL");
        let _ = sys::kill_service(pid, libc::SIGKILL);
    }

    Ok(())
}

/// Process control files written by the CLI. Called on SIGHUP.
/// Scans /run/arkhe/<name>/ctl/ for stop/start commands.
pub fn process_control_files(
    world: &mut World,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    for id in 0..world.len() {
        let name = world.names[id].clone();
        let ctl_dir = format!("/run/arkhe/{name}/ctl");

        let stop_file = format!("{ctl_dir}/stop");
        let start_file = format!("{ctl_dir}/start");

        if Path::new(&stop_file).exists() {
            let _ = std::fs::remove_file(&stop_file);
            eprintln!("arkhd: ctl: stop command for {name}");
            stop_service(world, id, ring)?;
        }

        if Path::new(&start_file).exists() {
            let _ = std::fs::remove_file(&start_file);
            eprintln!("arkhd: ctl: start command for {name}");
            world.run_configs[id].enabled = true;
            if matches!(world.states[id], RuntimeState::Stopped { .. } | RuntimeState::NotStarted) {
                world.states[id] = RuntimeState::NotStarted;
                world.log_pipe_fds[id] = None;
                world.log_file_fds[id] = None;
                write_state_files(&name, 0, "starting");
            }
        }
    }

    // Spawn any newly-startable services
    crate::systems::deps::spawn_ready_services(world, ring)?;
    Ok(())
}

/// Exponential backoff: 0, 0, 1, 2, 4, 8, 16, 32 (max) seconds.
fn backoff_secs(restart_count: u32) -> u64 {
    match restart_count {
        0 | 1 => 0,
        2 => 1,
        3 => 2,
        4 => 4,
        5 => 8,
        6 => 16,
        _ => 32,
    }
}

/// Write runtime state files for the CLI to read.
pub fn write_state_files(name: &str, pid: u32, state: &str) {
    let dir = format!("/run/arkhe/{name}");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(format!("{dir}/pid"), pid.to_string());
    let _ = std::fs::write(format!("{dir}/state"), state);
    if state == "running" {
        let epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let _ = std::fs::write(format!("{dir}/started"), epoch.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_schedule() {
        assert_eq!(backoff_secs(0), 0);
        assert_eq!(backoff_secs(1), 0);
        assert_eq!(backoff_secs(2), 1);
        assert_eq!(backoff_secs(3), 2);
        assert_eq!(backoff_secs(4), 4);
        assert_eq!(backoff_secs(5), 8);
        assert_eq!(backoff_secs(6), 16);
        assert_eq!(backoff_secs(7), 32);
        assert_eq!(backoff_secs(100), 32);
    }

    #[test]
    fn state_transitions() {
        let running = RuntimeState::Running { pid: 42, started_at: Instant::now() };
        assert!(matches!(running, RuntimeState::Running { pid: 42, .. }));
        let stopped = RuntimeState::Stopped { exit_code: Some(0), signal: None };
        assert!(matches!(stopped, RuntimeState::Stopped { exit_code: Some(0), .. }));
    }
}
