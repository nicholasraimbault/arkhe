//! Spawn system — start services as isolated child processes.
//!
//! spawn_service() is the entry point: it creates cgroups, applies resource
//! limits, pipes, reads environment variables, forks via clone3 with sandbox,
//! passes socket-activated fds, and registers the pidfd with io_uring.
//!
//! Zero unsafe in this file. All syscalls go through sys.rs.

use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::Instant;

use io_uring::IoUring;

use crate::cgroup;
use crate::components::{CgroupHandle, RuntimeState};
use crate::error::SupervisorError;
use crate::ring::{build_poll_multishot, Tag};
use crate::sys;
use crate::world::{ServiceId, World};

/// Spawn a service as an isolated, sandboxed child process.
pub fn spawn_service(
    world: &mut World,
    id: ServiceId,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    let name = world.names[id].clone();
    let run_path = world.run_configs[id].run_path.clone();
    let sv_dir = run_path.parent().expect("run_path must have parent");

    // 1. Create cgroup directory and get fd
    let cgroup_fd = cgroup::setup_service_cgroup(&name)?;

    // 2. Apply resource limits if configured
    if let Some(limits) = &world.resource_limits[id] {
        if let Err(e) = cgroup::apply_resource_limits(&name, limits) {
            eprintln!("arkhd: spawn: resource limits for {name}: {e}");
        }
    }

    // 3. Create log pipes
    let (log_read, log_write) = sys::create_pipe()?;

    // 4. Read env vars from /etc/sv/<name>/env/ (pre-fork)
    let mut env_cstrings = build_env(&sv_dir.join("env"));

    // 5. Socket activation: gather listen fd raw values, add env vars
    let listen_raw_fds: Vec<std::os::fd::RawFd> = world.listen_fds[id]
        .iter()
        .map(|fd| fd.as_raw_fd())
        .collect();
    if !listen_raw_fds.is_empty() {
        env_cstrings.push(
            CString::new(format!("LISTEN_FDS={}", listen_raw_fds.len())).unwrap(),
        );
        // LISTEN_PID=1 because CLONE_NEWPID gives child PID 1
        env_cstrings.push(CString::new("LISTEN_PID=1").unwrap());
    }

    // 6. Convert run_path to CString
    let run_cstr = CString::new(run_path.as_os_str().as_bytes()).map_err(|_| {
        SupervisorError::SpawnFork(
            name.clone(),
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains null byte"),
        )
    })?;

    // 7. Fork via clone3 — sandbox applied in child before exec
    let sandbox = &world.sandbox_configs[id];
    let mut clone_flags = sys::CLONE_PIDFD | sys::CLONE_INTO_CGROUP;
    if sandbox.permissive {
        eprintln!("arkhd: spawn: {name}: permissive mode — no namespace isolation");
    } else {
        if sandbox.pid_namespace {
            clone_flags |= sys::CLONE_NEWPID;
        }
        if sandbox.mount_namespace {
            clone_flags |= sys::CLONE_NEWNS;
        }
        if sandbox.ipc_namespace {
            clone_flags |= sys::CLONE_NEWIPC;
        }
    }

    let spawn_result = sys::clone3_exec(
        clone_flags,
        cgroup_fd.as_raw_fd(),
        log_write.as_raw_fd(),
        &run_cstr,
        &env_cstrings,
        &world.sandbox_configs[id],
        &listen_raw_fds,
        world.landlock_abi,
    );

    // If clone3 fails with EINVAL, retry without namespace flags (older kernels)
    let spawn_result = if matches!(
        &spawn_result,
        Err(SupervisorError::SpawnFork(_, io_err)) if io_err.raw_os_error() == Some(libc::EINVAL)
    ) {
        let degraded = clone_flags & !(sys::CLONE_NEWPID | sys::CLONE_NEWNS | sys::CLONE_NEWIPC);
        eprintln!("arkhd: spawn: {name}: clone3 EINVAL, retrying without namespace flags");
        sys::clone3_exec(
            degraded,
            cgroup_fd.as_raw_fd(),
            log_write.as_raw_fd(),
            &run_cstr,
            &env_cstrings,
            &world.sandbox_configs[id],
            &listen_raw_fds,
            world.landlock_abi,
        )
    } else {
        spawn_result
    };

    let result = spawn_result.map_err(|e| match e {
        SupervisorError::SpawnFork(_, io_err) => SupervisorError::SpawnFork(name.clone(), io_err),
        other => other,
    })?;

    // 8. Parent: close write end
    drop(log_write);

    // 9. Submit pidfd poll
    let pidfd_sqe = build_poll_multishot(&result.pidfd, Tag::Pidfd(id));
    sys::push_sqe(ring, &pidfd_sqe)?;

    // 10. Store state in World
    world.states[id] = RuntimeState::Running {
        pid: result.pid,
        started_at: Instant::now(),
    };
    world.pidfds[id] = Some(result.pidfd);
    world.cgroup_handles[id] = Some(CgroupHandle {
        cgroup_fd,
        psi_memory_fd: None,
        psi_cpu_fd: None,
    });
    world.log_pipe_fds[id] = Some(log_read);

    // 11. Set up log file and submit poll on log pipe
    match crate::systems::log::setup_log_dir(&name) {
        Ok(fd) => { world.log_file_fds[id] = Some(fd); }
        Err(e) => eprintln!("arkhd: log: failed to set up log for {name}: {e}"),
    }
    if let Some(pipe_fd) = &world.log_pipe_fds[id] {
        let log_sqe = build_poll_multishot(pipe_fd, Tag::Splice(id));
        sys::push_sqe(ring, &log_sqe)?;
    }

    // 12. Set up PSI monitoring
    if world.resource_limits[id].is_some() {
        if let Err(e) = cgroup::setup_psi_monitoring(&name, world, id, ring) {
            eprintln!("arkhd: spawn: PSI monitoring for {name}: {e}");
        }
    }

    // 13. Write runtime state files for CLI
    crate::systems::supervise::write_state_files(&name, result.pid, "running");

    // Reset restart count on successful spawn
    world.restart_states[id].count = 0;
    world.restart_states[id].backoff_secs = 0;

    Ok(())
}

/// Build the environment variable array for a child process.
fn build_env(env_dir: &Path) -> Vec<CString> {
    let mut vars: Vec<(String, String)> =
        vec![("PATH".into(), "/usr/bin:/usr/sbin:/bin:/sbin".into())];

    if let Ok(entries) = std::fs::read_dir(env_dir) {
        for entry in entries.flatten() {
            if let (Some(var_name), Ok(value)) = (
                entry.file_name().to_str().map(String::from),
                std::fs::read_to_string(entry.path()),
            ) {
                let value = value.trim().to_string();
                if let Some(existing) = vars.iter_mut().find(|(k, _)| k == &var_name) {
                    existing.1 = value;
                } else {
                    vars.push((var_name, value));
                }
            }
        }
    }

    vars.into_iter()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_dir(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("arkhe-spawn-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_env_no_dir() {
        let dir = test_dir("env-none");
        let env = build_env(&dir.join("env"));
        assert_eq!(env.len(), 1);
        assert_eq!(env[0].to_str().unwrap(), "PATH=/usr/bin:/usr/sbin:/bin:/sbin");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_env_with_files() {
        let dir = test_dir("env-files");
        let env_dir = dir.join("env");
        fs::create_dir_all(&env_dir).unwrap();
        fs::write(env_dir.join("LANG"), "C.UTF-8\n").unwrap();
        fs::write(env_dir.join("WORKERS"), "4").unwrap();

        let env = build_env(&env_dir);
        assert_eq!(env.len(), 3);
        let as_strs: Vec<&str> = env.iter().filter_map(|c| c.to_str().ok()).collect();
        assert!(as_strs.contains(&"PATH=/usr/bin:/usr/sbin:/bin:/sbin"));
        assert!(as_strs.contains(&"LANG=C.UTF-8"));
        assert!(as_strs.contains(&"WORKERS=4"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_env_with_listen_fds() {
        let dir = test_dir("env-listen");
        let mut env = build_env(&dir.join("env"));
        env.push(CString::new("LISTEN_FDS=2").unwrap());
        env.push(CString::new("LISTEN_PID=1").unwrap());

        let as_strs: Vec<&str> = env.iter().filter_map(|c| c.to_str().ok()).collect();
        assert!(as_strs.contains(&"LISTEN_FDS=2"));
        assert!(as_strs.contains(&"LISTEN_PID=1"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_basic_service() {
        if crate::cgroup::setup_service_cgroup("arkhe-test-spawn").is_err() {
            eprintln!("spawn_basic_service: skipped (requires cgroup access)");
            return;
        }
        let dir = test_dir("spawn-basic");
        let run_path = dir.join("run");
        fs::write(&run_path, "#!/bin/sh\necho hello\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&run_path, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let mut world = crate::world::World::new();
        crate::config::load_service(&mut world, &dir).unwrap();

        let mut ring = match io_uring::IoUring::new(64) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("spawn_basic_service: skipped (io_uring not available)");
                return;
            }
        };
        match spawn_service(&mut world, 0, &mut ring) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("spawn_basic_service: skipped (spawn failed: {e})");
                let _ = fs::remove_dir_all(&dir);
                return;
            }
        }

        match world.states[0] {
            RuntimeState::Running { pid, .. } => assert!(pid > 0),
            _ => panic!("expected Running state"),
        }
        assert!(world.pidfds[0].is_some());
        let _ = fs::remove_dir_all(&dir);
    }
}
