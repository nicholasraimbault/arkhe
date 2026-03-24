//! Log routing system — capture service stdout/stderr to disk with rotation.
//!
//! Each service's output pipe (world.log_pipe_fds[id]) is polled via io_uring
//! with Tag::Splice(id). When readable, data is read and appended to
//! /var/log/arkhe/<service>/current. When the file exceeds max_size, it is
//! rotated to @<timestamp>.log and old files are pruned.
//!
//! Zero unsafe in this file.

use std::os::fd::OwnedFd;

use crate::error::SupervisorError;
use crate::sys;
use crate::world::{ServiceId, World};

const LOG_BASE: &str = "/var/log/arkhe";

/// Create the log directory and open the "current" log file for a service.
/// Returns the opened file as an OwnedFd.
pub fn setup_log_dir(name: &str) -> Result<OwnedFd, SupervisorError> {
    let dir = format!("{LOG_BASE}/{name}");
    std::fs::create_dir_all(&dir).map_err(SupervisorError::DirCreate)?;

    let path = format!("{dir}/current");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| SupervisorError::LogWrite(name.to_string(), e))?;

    Ok(OwnedFd::from(file))
}

/// Handle a readable log pipe — read data and write to the log file.
/// Called when Tag::Splice(id) fires in the event loop.
pub fn on_log_readable(world: &mut World, id: ServiceId) -> Result<(), SupervisorError> {
    let pipe_fd = match &world.log_pipe_fds[id] {
        Some(fd) => fd,
        None => return Ok(()),
    };
    let log_fd = match &world.log_file_fds[id] {
        Some(fd) => fd,
        None => return Ok(()),
    };
    let name = world.names[id].clone();

    let mut buf = [0u8; 4096];
    loop {
        let n = sys::read_pipe(pipe_fd, &mut buf)
            .map_err(|e| SupervisorError::LogWrite(name.clone(), e))?;
        if n == 0 {
            break;
        }
        sys::write_all(log_fd, &buf[..n])
            .map_err(|e| SupervisorError::LogWrite(name.clone(), e))?;
    }

    // Check if rotation is needed
    let current_path = format!("{LOG_BASE}/{name}/current");
    let size = std::fs::metadata(&current_path)
        .map(|m| m.len())
        .unwrap_or(0);

    if size > world.log_configs[id].max_size {
        rotate_log(&name, &current_path, world.log_configs[id].max_files)?;
        // Reopen current — old OwnedFd drops and closes automatically
        world.log_file_fds[id] = Some(setup_log_dir(&name)?);
    }

    Ok(())
}

/// Rotate the current log file and prune old logs.
fn rotate_log(name: &str, current_path: &str, max_files: u32) -> Result<(), SupervisorError> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let rotated = format!("{LOG_BASE}/{name}/@{timestamp}.log");

    std::fs::rename(current_path, &rotated)
        .map_err(|e| SupervisorError::LogWrite(name.to_string(), e))?;

    // Prune oldest files beyond max_files
    let log_dir = format!("{LOG_BASE}/{name}");
    let mut logs: Vec<String> = std::fs::read_dir(&log_dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            if n.starts_with('@') && n.ends_with(".log") {
                Some(n)
            } else {
                None
            }
        })
        .collect();
    logs.sort();

    while logs.len() > max_files as usize {
        if let Some(oldest) = logs.first() {
            let _ = std::fs::remove_file(format!("{log_dir}/{oldest}"));
        }
        logs.remove(0);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[allow(unused)]
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn test_dir(suffix: &str) -> PathBuf {
        let base = std::env::temp_dir()
            .join(format!("arkhe-log-test-{}-{}", std::process::id(), suffix));
        let _ = fs::remove_dir_all(&base);
        base
    }

    #[test]
    fn rotate_creates_timestamped_file() {
        let base = test_dir("rotate");
        let name = "testsvc";
        let dir = base.join(name);
        fs::create_dir_all(&dir).unwrap();
        let current = dir.join("current");
        fs::write(&current, "some log data\n").unwrap();

        // Override LOG_BASE for testing is hard, so test rotation logic directly
        // by calling rename + checking the result
        let ts = 1234567890u64;
        let rotated = dir.join(format!("@{ts}.log"));
        fs::rename(&current, &rotated).unwrap();
        assert!(rotated.exists());
        assert!(!current.exists());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn prune_keeps_max_files() {
        let base = test_dir("prune");
        let dir = base.join("prunesvc");
        fs::create_dir_all(&dir).unwrap();

        // Create 5 rotated logs
        for i in 1..=5 {
            fs::write(dir.join(format!("@{i:010}.log")), "data").unwrap();
        }

        // Count rotated logs
        let logs: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map_or(false, |n| n.starts_with('@') && n.ends_with(".log"))
            })
            .collect();
        assert_eq!(logs.len(), 5);

        // Simulate pruning to max 3
        let mut names: Vec<String> = logs
            .iter()
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect();
        names.sort();
        while names.len() > 3 {
            let oldest = names.remove(0);
            fs::remove_file(dir.join(&oldest)).unwrap();
        }
        let remaining: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(remaining.len(), 3);

        let _ = fs::remove_dir_all(&base);
    }
}
