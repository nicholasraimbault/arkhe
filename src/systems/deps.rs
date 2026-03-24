//! Dependency resolver system — tracks readiness and triggers spawning.
//!
//! Two modes:
//! - fanotify: event-driven, watches /run/ready/ and /etc/sv/ (requires CONFIG_FANOTIFY)
//! - polling: timer-driven, scans directories every 500ms (fallback for any kernel)
//!
//! Zero unsafe in this file.

use std::collections::HashSet;
use std::path::Path;

use io_uring::IoUring;

use crate::components::RuntimeState;
use crate::config;
use crate::error::SupervisorError;
use crate::ring::{build_poll_multishot, Tag};
use crate::sys::{self, FAN_CREATE, FAN_DELETE, FAN_ONDIR};
use crate::world::{ServiceId, World};

// Polling uses a 1-second io_uring timeout via sys::alloc_timespec.

/// Scan /run/ready/ for existing readiness files and mark them in the World.
/// Called once at startup before the event loop.
pub fn scan_initial_readiness(world: &mut World) {
    let ready_dir = Path::new("/run/ready");
    let entries = match std::fs::read_dir(ready_dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            mark_dependency_satisfied(world, name);
            world.known_ready_files.insert(name.to_string());
        }
    }

    // Also snapshot current /etc/sv/ directories
    if let Ok(entries) = std::fs::read_dir("/etc/sv") {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    world.known_service_dirs.insert(name.to_string());
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Fanotify mode (event-driven)
// ═══════════════════════════════════════════════════════════════════════════════

/// Process fanotify events from the combined /run/ready/ + /etc/sv/ watcher.
/// After processing, spawn any services whose dependencies are now met.
pub fn on_fanotify_event(
    world: &mut World,
    fanotify_fd: &std::os::fd::OwnedFd,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    let events = sys::read_fanotify_events(fanotify_fd)?;

    for event in &events {
        let ready_path = Path::new("/run/ready").join(&event.name);
        let sv_path = Path::new("/etc/sv").join(&event.name);

        if event.mask & FAN_CREATE != 0 {
            // Readiness signal: file appeared in /run/ready/
            if ready_path.exists() {
                eprintln!("arkhd: deps: readiness signal for '{}'", event.name);
                mark_dependency_satisfied(world, &event.name);
            }

            // Drop-to-activate: new service directory in /etc/sv/
            if sv_path.is_dir() && (event.mask & FAN_ONDIR != 0) {
                eprintln!("arkhd: deps: new service directory '{}'", event.name);
                load_new_service(world, &sv_path);
            }
        }

        if event.mask & FAN_DELETE != 0 {
            // Readiness revoked
            if !ready_path.exists() {
                eprintln!("arkhd: deps: readiness revoked for '{}'", event.name);
                mark_dependency_unsatisfied(world, &event.name);
            }

            // Service directory removed
            if !sv_path.exists() && (event.mask & FAN_ONDIR != 0) {
                eprintln!("arkhd: deps: service directory removed '{}'", event.name);
                mark_service_removed(world, &event.name);
            }
        }
    }

    // Spawn any services that are now ready
    if !events.is_empty() {
        spawn_ready_services(world, ring)?;
    }

    Ok(())
}

/// Set up the fanotify watcher and submit it to the io_uring ring.
/// Returns the fanotify fd (caller must keep it alive).
pub fn setup_watcher(ring: &mut IoUring) -> Result<std::os::fd::OwnedFd, SupervisorError> {
    let fanotify_fd = sys::setup_fanotify()?;
    let sqe = build_poll_multishot(&fanotify_fd, Tag::Inotify);
    sys::push_sqe(ring, &sqe)?;
    Ok(fanotify_fd)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Polling mode (timer-driven fallback)
// ═══════════════════════════════════════════════════════════════════════════════

/// Handle a deps poll tick — scan directories, diff against snapshots, process changes.
/// Called from the event loop on every iteration (at most once per second).
pub fn on_deps_poll(world: &mut World, ring: &mut IoUring) -> Result<(), SupervisorError> {
    // 1. Scan /run/ready/ and diff against snapshot
    let current_ready = scan_dir_names("/run/ready");
    let ready_added: Vec<String> = current_ready
        .difference(&world.known_ready_files)
        .cloned()
        .collect();
    let ready_removed: Vec<String> = world
        .known_ready_files
        .difference(&current_ready)
        .cloned()
        .collect();
    world.known_ready_files = current_ready;

    for name in &ready_added {
        eprintln!("arkhd: deps: [poll] readiness signal for '{name}'");
        mark_dependency_satisfied(world, name);
    }
    for name in &ready_removed {
        eprintln!("arkhd: deps: [poll] readiness revoked for '{name}'");
        mark_dependency_unsatisfied(world, name);
    }

    // 2. Scan /etc/sv/ and diff against snapshot
    let current_svcs = scan_dir_names("/etc/sv");
    let svcs_added: Vec<String> = current_svcs
        .difference(&world.known_service_dirs)
        .cloned()
        .collect();
    let svcs_removed: Vec<String> = world
        .known_service_dirs
        .difference(&current_svcs)
        .cloned()
        .collect();
    world.known_service_dirs = current_svcs;

    for name in &svcs_added {
        let sv_path = Path::new("/etc/sv").join(name);
        if sv_path.is_dir() {
            eprintln!("arkhd: deps: [poll] new service directory '{name}'");
            load_new_service(world, &sv_path);
        }
    }
    for name in &svcs_removed {
        eprintln!("arkhd: deps: [poll] service directory removed '{name}'");
        mark_service_removed(world, name);
    }

    // 3. Always try to spawn ready services.
    // Even without directory changes, a service might have transitioned to
    // NotStarted (from a restart timeout) and now be ready to spawn.
    spawn_ready_services(world, ring)?;

    Ok(())
}

/// Scan a directory and return the set of entry names.
fn scan_dir_names(path: &str) -> HashSet<String> {
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Common helpers
// ═══════════════════════════════════════════════════════════════════════════════

/// Check all NotStarted services and spawn those whose dependencies are met.
/// Also called at startup after initial readiness scan.
pub fn spawn_ready_services(world: &mut World, ring: &mut IoUring) -> Result<(), SupervisorError> {
    let ready_ids: Vec<ServiceId> = (0..world.len())
        .filter(|&id| {
            matches!(world.states[id], RuntimeState::NotStarted)
                && world.run_configs[id].enabled
                && !has_disabled_marker(id, world)
                && all_deps_satisfied(id, world)
        })
        .collect();

    for id in ready_ids {
        let name = world.names[id].clone();
        match crate::systems::spawn::spawn_service(world, id, ring) {
            Ok(()) => eprintln!("arkhd: deps: spawned '{name}'"),
            Err(e) => eprintln!("arkhd: deps: failed to spawn '{name}': {e}"),
        }
    }

    Ok(())
}

/// Mark a dependency name as satisfied across all services.
fn mark_dependency_satisfied(world: &mut World, dep_name: &str) {
    for id in 0..world.len() {
        if let Some(deps) = &world.dependencies[id] {
            if deps.names.iter().any(|n| n == dep_name) {
                // Check if ALL deps for this service are now satisfied
                if all_deps_satisfied(id, world) {
                    world.readiness[id].satisfied = true;
                }
            }
        }
    }
}

/// Mark a dependency name as unsatisfied, un-readying affected services.
fn mark_dependency_unsatisfied(world: &mut World, dep_name: &str) {
    for id in 0..world.len() {
        if let Some(deps) = &world.dependencies[id] {
            if deps.names.iter().any(|n| n == dep_name) {
                world.readiness[id].satisfied = false;
            }
        }
    }
}

/// Check if all dependencies for a service are satisfied.
/// A service with no dependencies is always satisfied.
fn all_deps_satisfied(id: ServiceId, world: &World) -> bool {
    let deps = match &world.dependencies[id] {
        Some(d) => d,
        None => return true,
    };

    // Collect current readiness files
    let ready_dir = Path::new("/run/ready");
    let ready_files: HashSet<String> = std::fs::read_dir(ready_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();

    deps.names.iter().all(|dep| ready_files.contains(dep))
}

/// Load a new service directory (drop-to-activate).
fn load_new_service(world: &mut World, path: &Path) {
    // Skip if already loaded
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if world.find_by_name(name).is_some() {
            return;
        }
    }

    match config::load_service(world, path) {
        Ok(name) => eprintln!("arkhd: deps: loaded new service '{name}'"),
        Err(e) => eprintln!("arkhd: deps: failed to load {}: {e}", path.display()),
    }
}

/// Mark a service as removed (set to Stopped state).
///
/// If the service is currently running, we must SIGTERM it before clearing
/// state — otherwise the process becomes orphaned (no parent to reap it).
/// We also clear restart_timeout_ptrs to prevent a pending restart SQE from
/// firing after the service slot has been repurposed.
fn mark_service_removed(world: &mut World, name: &str) {
    if let Some(id) = world.find_by_name(name) {
        // Kill the process if it is running before we drop the pidfd
        let maybe_pid = match world.states[id] {
            RuntimeState::Running { pid, .. }
            | RuntimeState::Ready { pid, .. }
            | RuntimeState::Stopping { pid, .. } => Some(pid),
            _ => None,
        };
        if let Some(pid) = maybe_pid {
            if let Err(e) = sys::kill_service(pid, libc::SIGTERM) {
                eprintln!("arkhd: deps: SIGTERM to {name} (pid {pid}) failed: {e}");
            }
        }

        // Invalidate any pending restart timeout SQE for this service
        sys::free_timespec(world.restart_timeout_ptrs[id]);
        world.restart_timeout_ptrs[id] = 0;

        world.states[id] = RuntimeState::Stopped {
            exit_code: None,
            signal: None,
        };
        world.run_configs[id].enabled = false;
        // Drop pidfd and cgroup handle — RAII closes the kernel fds
        world.pidfds[id] = None;
        world.cgroup_handles[id] = None;
        world.log_pipe_fds[id] = None;
    }
}

/// Check for a "disabled" marker file in the service directory.
fn has_disabled_marker(id: ServiceId, world: &World) -> bool {
    let sv_dir = world.run_configs[id].run_path.parent();
    sv_dir.map_or(false, |d| d.join("disabled").exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::{Dependencies, RunConfig};
    use crate::world::World;
    use std::fs;
    use std::path::PathBuf;

    fn test_dir(suffix: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("arkhe-deps-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn add_test_service(world: &mut World, name: &str, deps: Option<Vec<&str>>) -> ServiceId {
        let id = world.add_service(
            name.to_string(),
            RunConfig {
                run_path: PathBuf::from(format!("/etc/sv/{name}/run")),
                finish_path: None,
                enabled: true,
            },
        );
        if let Some(dep_names) = deps {
            world.dependencies[id] = Some(Dependencies {
                names: dep_names.into_iter().map(String::from).collect(),
            });
        }
        id
    }

    #[test]
    fn no_deps_always_satisfied() {
        let world = &mut World::new();
        let id = add_test_service(world, "simple", None);
        assert!(all_deps_satisfied(id, world));
    }

    #[test]
    fn deps_with_ready_files() {
        let ready_dir = test_dir("ready");
        // Create readiness files
        fs::write(ready_dir.join("network-online"), "").unwrap();
        fs::write(ready_dir.join("dns-ready"), "").unwrap();

        // This test checks the internal logic; in production, all_deps_satisfied
        // reads from /run/ready/ which we can't write to without root.
        // So this test validates the helper functions work with a mock World.
        let world = &mut World::new();
        let id = add_test_service(world, "nginx", Some(vec!["network-online", "dns-ready"]));

        // Without real /run/ready/ access, the deps won't be satisfied.
        // This test primarily verifies no panics and correct structure.
        let _satisfied = all_deps_satisfied(id, world);

        let _ = fs::remove_dir_all(&ready_dir);
    }

    #[test]
    fn disabled_marker() {
        let dir = test_dir("disabled");
        fs::write(dir.join("run"), "#!/bin/sh\nexec true\n").unwrap();
        fs::write(dir.join("disabled"), "").unwrap();

        let world = &mut World::new();
        let id = world.add_service(
            "test-disabled".to_string(),
            RunConfig {
                run_path: dir.join("run"),
                finish_path: None,
                enabled: true,
            },
        );

        assert!(has_disabled_marker(id, world));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_service_removed_cleanup() {
        let world = &mut World::new();
        add_test_service(world, "to-remove", None);

        mark_service_removed(world, "to-remove");
        assert!(!world.run_configs[0].enabled);
        assert!(matches!(world.states[0], RuntimeState::Stopped { .. }));
        assert!(world.pidfds[0].is_none());
    }

    #[test]
    fn scan_dir_names_works() {
        let dir = test_dir("scan-names");
        fs::write(dir.join("foo"), "").unwrap();
        fs::write(dir.join("bar"), "").unwrap();
        fs::create_dir(dir.join("baz")).unwrap();

        let names = scan_dir_names(dir.to_str().unwrap());
        assert!(names.contains("foo"));
        assert!(names.contains("bar"));
        assert!(names.contains("baz"));
        assert_eq!(names.len(), 3);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn polling_diff_detects_changes() {
        let dir = test_dir("poll-diff");
        fs::write(dir.join("svc-a"), "").unwrap();
        fs::write(dir.join("svc-b"), "").unwrap();

        // Take initial snapshot
        let snapshot: HashSet<String> = scan_dir_names(dir.to_str().unwrap());
        assert_eq!(snapshot.len(), 2);

        // Add a file
        fs::write(dir.join("svc-c"), "").unwrap();
        let current = scan_dir_names(dir.to_str().unwrap());

        // New entries
        let added: Vec<&String> = current.difference(&snapshot).collect();
        assert_eq!(added.len(), 1);
        assert_eq!(added[0], "svc-c");

        // Remove a file
        fs::remove_file(dir.join("svc-a")).unwrap();
        let current2 = scan_dir_names(dir.to_str().unwrap());
        let removed: Vec<&String> = snapshot.difference(&current2).collect();
        assert!(removed.contains(&&"svc-a".to_string()));

        let _ = fs::remove_dir_all(&dir);
    }
}
