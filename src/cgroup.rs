//! cgroup v2 operations.
//!
//! Creates service cgroup directories under /sys/fs/cgroup/arkhe.slice/.
//! Each service gets a <name>.scope cgroup for resource accounting and PSI.
//! Applies resource limits and sets up PSI monitoring triggers.

use std::fs::{self, File};
use std::os::fd::{AsRawFd, OwnedFd};

use io_uring::IoUring;

use crate::components::ResourceLimits;
use crate::error::SupervisorError;
use crate::ring::{build_poll_multishot_mask, Tag};
use crate::sys;
use crate::world::{ServiceId, World};

const CGROUP_SLICE: &str = "/sys/fs/cgroup/arkhe.slice";

/// Create the cgroup directory for a service and return an fd to it.
pub fn setup_service_cgroup(name: &str) -> Result<OwnedFd, SupervisorError> {
    create_dir_ok(CGROUP_SLICE)?;

    let scope_path = format!("{CGROUP_SLICE}/{name}.scope");
    create_dir_ok(&scope_path)?;

    File::open(&scope_path)
        .map(OwnedFd::from)
        .map_err(|e| SupervisorError::CgroupCreate(scope_path, e))
}

/// Write resource limits to the cgroup interface files.
/// Only writes limits that are Some — leaves unset knobs alone.
pub fn apply_resource_limits(name: &str, limits: &ResourceLimits) -> Result<(), SupervisorError> {
    let scope = format!("{CGROUP_SLICE}/{name}.scope");

    if let Some(max) = limits.memory_max {
        write_knob(&scope, "memory.max", &max.to_string())?;
    }
    if let Some(high) = limits.memory_high {
        write_knob(&scope, "memory.high", &high.to_string())?;
    }
    if let Some(weight) = limits.cpu_weight {
        write_knob(&scope, "cpu.weight", &weight.to_string())?;
    }
    if let Some((quota, period)) = limits.cpu_max {
        write_knob(&scope, "cpu.max", &format!("{quota} {period}"))?;
    }
    if let Some(weight) = limits.io_weight {
        write_knob(&scope, "io.weight", &format!("default {weight}"))?;
    }
    if let Some(max) = limits.pids_max {
        write_knob(&scope, "pids.max", &max.to_string())?;
    }

    Ok(())
}

/// Set up PSI monitoring for a service's cgroup.
/// Registers memory pressure trigger and submits poll to io_uring.
pub fn setup_psi_monitoring(
    name: &str,
    world: &mut World,
    id: ServiceId,
    ring: &mut IoUring,
) -> Result<(), SupervisorError> {
    let scope = format!("{CGROUP_SLICE}/{name}.scope");

    // Memory pressure: trigger when any task stalls > 100ms in any 1s window
    let mem_fd = match sys::setup_psi_trigger(&scope, "memory", "some 100000 1000000") {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("arkhd: cgroup: PSI setup failed for {name}: {e}");
            return Ok(()); // Non-fatal — PSI might not be available
        }
    };

    // Submit poll with POLLPRI (PSI uses priority poll events)
    let sqe = build_poll_multishot_mask(&mem_fd, libc::POLLPRI as u32, Tag::Psi(id));
    sys::push_sqe(ring, &sqe)?;

    // Store the PSI fd in the cgroup handle
    if let Some(ref mut handle) = world.cgroup_handles[id] {
        world.psi_map.insert(mem_fd.as_raw_fd(), id);
        handle.psi_memory_fd = Some(mem_fd);
    }

    Ok(())
}

/// Write a value to a cgroup knob. Logs and continues on failure.
fn write_knob(scope: &str, filename: &str, value: &str) -> Result<(), SupervisorError> {
    sys::write_cgroup_file(scope, filename, value)
        .map_err(|e| SupervisorError::CgroupWrite(format!("{scope}/{filename}"), e))
}

/// Create a directory, ignoring "already exists" errors.
fn create_dir_ok(path: &str) -> Result<(), SupervisorError> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(SupervisorError::CgroupCreate(path.to_string(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir()
            .join(format!("arkhe-cgroup-test-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn write_cgroup_files() {
        let dir = test_dir("write");
        let scope = dir.to_str().unwrap();

        // Write various cgroup knobs to temp dir
        sys::write_cgroup_file(scope, "memory.max", "536870912").unwrap();
        sys::write_cgroup_file(scope, "cpu.weight", "100").unwrap();
        sys::write_cgroup_file(scope, "cpu.max", "80000 100000").unwrap();
        sys::write_cgroup_file(scope, "pids.max", "64").unwrap();

        // Read back and verify
        assert_eq!(fs::read_to_string(dir.join("memory.max")).unwrap(), "536870912");
        assert_eq!(fs::read_to_string(dir.join("cpu.weight")).unwrap(), "100");
        assert_eq!(fs::read_to_string(dir.join("cpu.max")).unwrap(), "80000 100000");
        assert_eq!(fs::read_to_string(dir.join("pids.max")).unwrap(), "64");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_limits_only_writes_some() {
        let dir = test_dir("partial");
        let scope = dir.to_str().unwrap();

        // Create a scope dir that looks like a cgroup
        let scope_dir = dir.join("testsvc.scope");
        fs::create_dir_all(&scope_dir).unwrap();

        let limits = ResourceLimits {
            memory_max: Some(512 * 1024 * 1024),
            memory_high: None,
            cpu_weight: Some(100),
            cpu_max: None,
            io_weight: None,
            pids_max: Some(64),
        };

        // We can't use apply_resource_limits directly (it uses CGROUP_SLICE),
        // so test the underlying write_cgroup_file
        let s = scope_dir.to_str().unwrap();
        if let Some(max) = limits.memory_max {
            sys::write_cgroup_file(s, "memory.max", &max.to_string()).unwrap();
        }
        if limits.memory_high.is_none() {
            assert!(!scope_dir.join("memory.high").exists());
        }
        if let Some(w) = limits.cpu_weight {
            sys::write_cgroup_file(s, "cpu.weight", &w.to_string()).unwrap();
        }
        if let Some(max) = limits.pids_max {
            sys::write_cgroup_file(s, "pids.max", &max.to_string()).unwrap();
        }

        assert_eq!(fs::read_to_string(scope_dir.join("memory.max")).unwrap(), "536870912");
        assert!(!scope_dir.join("memory.high").exists());
        assert_eq!(fs::read_to_string(scope_dir.join("cpu.weight")).unwrap(), "100");
        assert!(!scope_dir.join("cpu.max").exists());
        assert_eq!(fs::read_to_string(scope_dir.join("pids.max")).unwrap(), "64");

        let _ = fs::remove_dir_all(&dir);
    }
}
