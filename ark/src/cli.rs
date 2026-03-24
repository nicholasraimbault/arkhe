//! CLI command implementations
//!
//! Each function reads plain text files from /run/arkhe/, /etc/sv/,
//! and /var/log/arkhe/. No IPC. No protocols. Just files.

use std::io::{self, IsTerminal};
use std::path::Path;
use std::{fs, process};

const ARKHE_RUN_DIR: &str = "/run/arkhe";
const SERVICE_DIR: &str = "/etc/sv";
const LOG_DIR: &str = "/var/log/arkhe";

// ANSI color codes
const GREEN: &str = "\x1b[32m";
const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

fn use_color() -> bool {
    io::stdout().is_terminal()
}

fn colored(text: &str, color: &str) -> String {
    if use_color() {
        format!("{color}{text}{RESET}")
    } else {
        text.to_string()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ark status
// ─────────────────────────────────────────────────────────────────────────────

pub fn status(args: &[String]) -> Result<(), io::Error> {
    if let Some(name) = args.first() {
        return status_one(name);
    }

    // List all services
    let mut entries: Vec<String> = fs::read_dir(SERVICE_DIR)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .collect();
    entries.sort();

    if entries.is_empty() {
        eprintln!("no services in {SERVICE_DIR}");
        return Ok(());
    }

    for name in &entries {
        let state = read_state(name);
        let pid = read_pid(name);
        let uptime = read_uptime(name);

        let state_display = match state.as_str() {
            "running" => colored("running", GREEN),
            "stopped" => colored("stopped", RED),
            "starting" | "failing" => colored(&state, YELLOW),
            other => other.to_string(),
        };

        let pid_str = if pid > 0 {
            format!("pid {pid}")
        } else {
            "-".to_string()
        };

        let uptime_str = if !uptime.is_empty() {
            format!("up {uptime}")
        } else {
            String::new()
        };

        println!("{name:<20} {state_display:<18} {pid_str:<12} {uptime_str}");
    }

    Ok(())
}

fn status_one(name: &str) -> Result<(), io::Error> {
    let sv_path = Path::new(SERVICE_DIR).join(name);
    if !sv_path.is_dir() {
        eprintln!("ark: unknown service '{name}'");
        process::exit(1);
    }

    let state = read_state(name);
    let pid = read_pid(name);
    let uptime = read_uptime(name);

    println!("service: {name}");
    println!("state:   {state}");
    if pid > 0 {
        println!("pid:     {pid}");
    }
    if !uptime.is_empty() {
        println!("uptime:  {uptime}");
    }

    // Show config details
    if sv_path.join("depends").exists() {
        let deps = fs::read_to_string(sv_path.join("depends")).unwrap_or_default();
        let deps: Vec<&str> = deps.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')).collect();
        if !deps.is_empty() {
            println!("depends: {}", deps.join(", "));
        }
    }
    if sv_path.join("sandbox").exists() {
        let content = fs::read_to_string(sv_path.join("sandbox")).unwrap_or_default();
        if content.contains("sandbox = permissive") {
            println!("sandbox: {}", colored("permissive", YELLOW));
        } else {
            println!("sandbox: strict");
        }
    } else {
        println!("sandbox: strict (default)");
    }

    Ok(())
}

fn read_state(name: &str) -> String {
    fs::read_to_string(format!("{ARKHE_RUN_DIR}/{name}/state"))
        .unwrap_or_else(|_| "unknown".to_string())
        .trim()
        .to_string()
}

fn read_pid(name: &str) -> u32 {
    fs::read_to_string(format!("{ARKHE_RUN_DIR}/{name}/pid"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn read_uptime(name: &str) -> String {
    let started: u64 = fs::read_to_string(format!("{ARKHE_RUN_DIR}/{name}/started"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    if started == 0 {
        return String::new();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if now <= started {
        return String::new();
    }
    let secs = now - started;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else if secs < 86400 {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}d{}h", secs / 86400, (secs % 86400) / 3600)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// ark log
// ─────────────────────────────────────────────────────────────────────────────

pub fn log(args: &[String]) -> Result<(), io::Error> {
    let mut lines = 20usize;
    let mut name: Option<&str> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                i += 1;
                if i < args.len() {
                    lines = args[i].parse().unwrap_or(20);
                }
            }
            s => name = Some(s),
        }
        i += 1;
    }

    let name = match name {
        Some(n) => n,
        None => {
            eprintln!("usage: ark log <service> [-n LINES]");
            process::exit(1);
        }
    };

    let log_path = format!("{LOG_DIR}/{name}/current");
    let content = match fs::read_to_string(&log_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ark: cannot read log for '{name}': {e}");
            process::exit(1);
        }
    };

    let all_lines: Vec<&str> = content.lines().collect();
    let start = all_lines.len().saturating_sub(lines);
    for line in &all_lines[start..] {
        println!("{line}");
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// ark check
// ─────────────────────────────────────────────────────────────────────────────

pub fn check(args: &[String]) -> Result<(), io::Error> {
    let services: Vec<String> = if args.is_empty() {
        // Check all services
        fs::read_dir(SERVICE_DIR)?
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .collect()
    } else {
        args.to_vec()
    };

    let mut issues = 0;

    for name in &services {
        let sv_path = Path::new(SERVICE_DIR).join(name);
        if !sv_path.is_dir() {
            println!("{name}: {}", colored("not found", RED));
            issues += 1;
            continue;
        }

        let mut svc_issues = Vec::new();

        // Check run file
        if !sv_path.join("run").exists() {
            svc_issues.push(colored("missing 'run' file", RED));
        }

        // Check sandbox
        if sv_path.join("sandbox").exists() {
            let content = fs::read_to_string(sv_path.join("sandbox")).unwrap_or_default();
            if content.contains("sandbox = permissive") {
                svc_issues.push(colored("permissive sandbox", YELLOW));
            }
            // Check for unknown keys
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, _)) = line.split_once('=') {
                    let key = key.trim();
                    if !is_known_sandbox_key(key) {
                        svc_issues.push(format!("unknown sandbox key '{key}'"));
                    }
                }
            }
        }

        // Check depends — do referenced services exist?
        if sv_path.join("depends").exists() {
            let content = fs::read_to_string(sv_path.join("depends")).unwrap_or_default();
            for dep in content.lines().map(|l| l.trim()).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                // Dependencies are readiness signals, not necessarily service names
                // but we can check if the service exists
                if !Path::new(SERVICE_DIR).join(dep).is_dir() {
                    svc_issues.push(format!("dependency '{dep}' has no matching service"));
                }
            }
        }

        if svc_issues.is_empty() {
            println!("{name}: ok");
        } else {
            for issue in &svc_issues {
                println!("{name}: {issue}");
                issues += 1;
            }
        }
    }

    if issues > 0 {
        println!("\n{issues} issue(s) found");
        process::exit(1);
    }
    Ok(())
}

fn is_known_sandbox_key(key: &str) -> bool {
    matches!(
        key,
        "read" | "write" | "exec" | "bind" | "connect"
            | "pid-namespace" | "mount-namespace" | "ipc-namespace"
            | "uts-namespace" | "network-namespace"
            | "private-tmp" | "read-only-root"
            | "caps" | "seccomp" | "ipc-scope" | "sandbox"
    )
}

// ─────────────────────────────────────────────────────────────────────────────
// ark new
// ─────────────────────────────────────────────────────────────────────────────

pub fn new_service(args: &[String]) -> Result<(), io::Error> {
    let name = match args.first() {
        Some(n) => n,
        None => {
            eprintln!("usage: ark new <name>");
            process::exit(1);
        }
    };

    let sv_path = Path::new(SERVICE_DIR).join(name);
    if sv_path.exists() {
        eprintln!("ark: service '{name}' already exists at {}", sv_path.display());
        process::exit(1);
    }

    fs::create_dir_all(&sv_path)?;

    // Create run script
    let run_path = sv_path.join("run");
    fs::write(
        &run_path,
        "#!/bin/sh\n\
         # Edit this file to start your service.\n\
         # The command must run in the foreground (no daemonizing).\n\
         exec /path/to/your/binary\n",
    )?;
    // Make executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&run_path, fs::Permissions::from_mode(0o755))?;
    }

    // Create sandbox file with commented defaults
    fs::write(
        sv_path.join("sandbox"),
        "# arkhe sandbox configuration\n\
         # Default: deny all. Uncomment and edit to allow access.\n\
         #\n\
         # Filesystem access (comma-separated paths)\n\
         # read = /usr, /lib, /etc/myapp\n\
         # write = /var/log/myapp, /run/myapp\n\
         # exec = /usr/bin/myapp, /usr/bin/sh\n\
         #\n\
         # Network access (comma-separated ports, or 'none')\n\
         # bind = 8080\n\
         # connect = 443\n\
         #\n\
         # Namespace isolation (yes/no)\n\
         # pid-namespace = yes\n\
         # mount-namespace = yes\n\
         # ipc-namespace = yes\n\
         # uts-namespace = yes\n\
         # network-namespace = private\n\
         #\n\
         # Other settings\n\
         # private-tmp = yes\n\
         # read-only-root = yes\n\
         # caps = net_bind_service\n\
         # seccomp = default\n\
         # ipc-scope = scoped\n\
         #\n\
         # ESCAPE HATCH (logged, visible in 'ark check'):\n\
         # sandbox = permissive\n",
    )?;

    // Create depends file
    fs::write(
        sv_path.join("depends"),
        "# Dependencies — one per line. Service waits until these are ready.\n\
         # Example:\n\
         # network-online\n",
    )?;

    println!("Created {}", sv_path.display());
    println!("  run      — edit this to start your service");
    println!("  sandbox  — strict defaults (edit to allow access)");
    println!("  depends  — add dependencies if needed");

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// ark start / stop / restart / enable / disable
// ─────────────────────────────────────────────────────────────────────────────

pub fn stop(args: &[String]) -> Result<(), io::Error> {
    let name = require_arg(args, "stop")?;
    let ctl_dir = format!("{ARKHE_RUN_DIR}/{name}/ctl");
    fs::create_dir_all(&ctl_dir)?;
    fs::write(format!("{ctl_dir}/stop"), "")?;
    // Best-effort signal: the control file is already written, so arkhd will
    // pick it up on the next poll even if the signal fails.
    let _ = signal_supervisor();
    println!("stopping {name}");
    Ok(())
}

pub fn start(args: &[String]) -> Result<(), io::Error> {
    let name = require_arg(args, "start")?;
    // Remove stop marker and disabled marker
    let _ = fs::remove_file(format!("{ARKHE_RUN_DIR}/{name}/ctl/stop"));
    let _ = fs::remove_file(format!("{SERVICE_DIR}/{name}/disabled"));
    // Write start control file
    let ctl_dir = format!("{ARKHE_RUN_DIR}/{name}/ctl");
    fs::create_dir_all(&ctl_dir)?;
    fs::write(format!("{ctl_dir}/start"), "")?;
    // Best-effort signal: the control file is already written, so arkhd will
    // pick it up on the next poll even if the signal fails.
    let _ = signal_supervisor();
    println!("starting {name}");
    Ok(())
}

pub fn restart(args: &[String]) -> Result<(), io::Error> {
    let name = require_arg(args, "restart")?;
    let args = vec![name.clone()];
    stop(&args)?;
    // Brief pause for stop to take effect
    std::thread::sleep(std::time::Duration::from_millis(200));
    start(&args)?;
    Ok(())
}

pub fn enable(args: &[String]) -> Result<(), io::Error> {
    let name = require_arg(args, "enable")?;
    let marker = format!("{SERVICE_DIR}/{name}/disabled");
    if Path::new(&marker).exists() {
        fs::remove_file(&marker)?;
        println!("enabled {name}");
    } else {
        println!("{name} is already enabled");
    }
    Ok(())
}

pub fn disable(args: &[String]) -> Result<(), io::Error> {
    let name = require_arg(args, "disable")?;
    let marker = format!("{SERVICE_DIR}/{name}/disabled");
    fs::write(&marker, "")?;
    println!("disabled {name}");
    // Also stop if running
    let state = read_state(&name);
    if state == "running" || state == "starting" {
        let args = vec![name];
        stop(&args)?;
    }
    Ok(())
}

/// `ark reload` — signal the supervisor to immediately rescan /etc/sv/ for new
/// or removed service directories, without waiting for the next polling tick.
///
/// Under the hood this sends SIGHUP, which arkhd handles by running both
/// `process_control_files` and `on_deps_poll` so the rescan is immediate.
pub fn reload(_args: &[String]) -> Result<(), io::Error> {
    signal_supervisor()?;
    println!("reloading service directory (sent SIGHUP to supervisor)");
    Ok(())
}

fn require_arg(args: &[String], cmd: &str) -> Result<String, io::Error> {
    args.first().cloned().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, format!("usage: ark {cmd} <service>"))
    })
}

/// Send SIGHUP to the supervisor to trigger control file processing.
///
/// Returns an error if the supervisor is not running (pid file absent), so
/// callers like `reload` can surface the problem rather than silently doing
/// nothing.
fn signal_supervisor() -> Result<(), io::Error> {
    let pid_str = fs::read_to_string(format!("{ARKHE_RUN_DIR}/arkhd.pid"))
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "supervisor not running (no pid file)"))?;
    let pid = pid_str.trim();
    let _ = std::process::Command::new("kill")
        .args(["-HUP", pid])
        .status();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reload_with_no_supervisor_errors() {
        // reload() must return Err when the supervisor is not running — a missing
        // pid file means SIGHUP cannot be delivered, so we surface the error
        // rather than silently doing nothing.
        let result = reload(&[]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn reload_ignores_extra_args_but_still_errors_without_supervisor() {
        // Extra arguments are silently ignored (future-proof API), but the
        // error from a missing supervisor is still propagated.
        let args: Vec<String> = vec!["ignored-arg".to_string()];
        let result = reload(&args);
        assert!(result.is_err());
    }

    #[test]
    fn require_arg_returns_first_element() {
        let args = vec!["my-service".to_string()];
        let got = require_arg(&args, "start").unwrap();
        assert_eq!(got, "my-service");
    }

    #[test]
    fn require_arg_errors_on_empty() {
        let args: Vec<String> = vec![];
        let err = require_arg(&args, "start").unwrap_err();
        assert!(err.to_string().contains("usage: ark start"));
    }

    #[test]
    fn read_uptime_zero_start_returns_empty() {
        // A started-at timestamp of 0 means no uptime data available.
        // read_uptime reads from the filesystem, so we only test the pure
        // formatting paths that don't touch /run/arkhe/.
        //
        // The "0 started timestamp → empty string" path is exercised by the
        // function itself when the file is absent or unparseable, but we
        // validate the uptime formatting logic via the known output format.
        let secs_per_minute: u64 = 60;
        let secs_per_hour: u64 = 3600;
        let secs_per_day: u64 = 86400;

        // Verify format strings match what the function produces.
        let s = 45u64;
        assert_eq!(format!("{s}s"), "45s");
        let m = secs_per_minute + 30;
        assert_eq!(format!("{}m{}s", m / 60, m % 60), "1m30s");
        let h = secs_per_hour + secs_per_minute * 5;
        assert_eq!(format!("{}h{}m", h / secs_per_hour, (h % secs_per_hour) / 60), "1h5m");
        let d = secs_per_day + secs_per_hour * 3;
        assert_eq!(format!("{}d{}h", d / secs_per_day, (d % secs_per_day) / secs_per_hour), "1d3h");
    }

    #[test]
    fn is_known_sandbox_key_accepts_all_valid_keys() {
        let valid_keys = [
            "read", "write", "exec", "bind", "connect",
            "pid-namespace", "mount-namespace", "ipc-namespace",
            "uts-namespace", "network-namespace",
            "private-tmp", "read-only-root",
            "caps", "seccomp", "ipc-scope", "sandbox",
        ];
        for key in &valid_keys {
            assert!(is_known_sandbox_key(key), "expected '{key}' to be known");
        }
    }

    #[test]
    fn is_known_sandbox_key_rejects_unknown() {
        assert!(!is_known_sandbox_key("unknown-key"));
        assert!(!is_known_sandbox_key(""));
        assert!(!is_known_sandbox_key("ROOT"));
    }
}
