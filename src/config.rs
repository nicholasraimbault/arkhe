//! Config module — parse /etc/sv/ directories into World components.
//!
//! Parsing rules (from SERVICE-FORMAT.md):
//! - Read file as string, split on newlines
//! - Skip lines starting with '#'
//! - Split on first '=' for key-value pairs
//! - Trim whitespace
//! - Split on ',' for list values
//!
//! Every parse error is operational — log it, skip that component, continue.
//! The supervisor never crashes because of a bad config file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::components::*;
use crate::error::SupervisorError;
use crate::world::World;

/// Load a single service from /etc/sv/<name>/.
///
/// Returns the service name on success for logging.
/// Only returns Err for fatal issues (invalid dir name, missing run file).
/// All optional config parse errors are logged and skipped.
pub fn load_service(world: &mut World, path: &Path) -> Result<String, SupervisorError> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_string())
        .ok_or_else(|| {
            SupervisorError::ConfigLoad(path.display().to_string(), "invalid directory name".into())
        })?;

    // run file is required
    let run_path = path.join("run");
    if !run_path.exists() {
        return Err(SupervisorError::ConfigLoad(
            name,
            "missing 'run' file".into(),
        ));
    }

    // finish file is optional
    let finish_path = path.join("finish");
    let finish = if finish_path.exists() {
        Some(finish_path)
    } else {
        None
    };

    let id = world.add_service(
        name.clone(),
        RunConfig {
            run_path,
            finish_path: finish,
            enabled: true,
        },
    );

    // Parse optional config files — errors logged, defaults kept
    if let Some(content) = read_opt(&path.join("depends")) {
        if let Some(deps) = parse_depends(&content) {
            world.dependencies[id] = Some(deps);
        }
    }

    if let Some(content) = read_opt(&path.join("sandbox")) {
        world.sandbox_configs[id] = parse_sandbox(&content, &name);
    }

    if let Some(content) = read_opt(&path.join("resources")) {
        world.resource_limits[id] = parse_resources(&content, &name);
    }

    if let Some(content) = read_opt(&path.join("listen")) {
        world.listen_sockets[id] = parse_listen(&content, &name);
    }

    if let Some(content) = read_opt(&path.join("ready")) {
        let (mode, timeout) = parse_ready(&content, &name);
        world.readiness[id].mode = mode;
        world.readiness[id].timeout = timeout;
    }

    if let Some(content) = read_opt(&path.join("log").join("config")) {
        world.log_configs[id] = parse_log_config(&content, &name);
    }

    Ok(name)
}

// === File I/O helpers ===

/// Read a file, returning None if it doesn't exist or can't be read.
fn read_opt(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

// === Parsing helpers ===

/// Parse key=value pairs from file content.
/// Skips blank lines and lines starting with '#'.
fn parse_kv(content: &str) -> impl Iterator<Item = (&str, &str)> {
    content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (k, v) = l.split_once('=')?;
            Some((k.trim(), v.trim()))
        })
}

/// Parse a boolean value: yes/no, true/false, 1/0.
fn parse_bool(val: &str) -> Option<bool> {
    match val {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

/// Parse a size value with optional K/M/G suffix.
fn parse_size(val: &str) -> Option<u64> {
    let val = val.trim();
    if let Some(n) = val.strip_suffix('G') {
        n.trim().parse::<u64>().ok().map(|n| n * 1024 * 1024 * 1024)
    } else if let Some(n) = val.strip_suffix('M') {
        n.trim().parse::<u64>().ok().map(|n| n * 1024 * 1024)
    } else if let Some(n) = val.strip_suffix('K') {
        n.trim().parse::<u64>().ok().map(|n| n * 1024)
    } else {
        val.parse::<u64>().ok()
    }
}

/// Parse comma-separated paths.
fn parse_path_list(val: &str) -> Vec<PathBuf> {
    val.split(',')
        .map(|s| PathBuf::from(s.trim()))
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

/// Parse comma-separated port numbers. "none" → empty list.
fn parse_port_list(val: &str) -> Vec<u16> {
    if val.trim() == "none" {
        return Vec::new();
    }
    val.split(',')
        .filter_map(|s| s.trim().parse::<u16>().ok())
        .collect()
}

/// Parse comma-separated capability names.
fn parse_cap_list(val: &str) -> Vec<String> {
    val.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// === Component parsers ===

/// Parse depends file: one dependency name per line.
fn parse_depends(content: &str) -> Option<Dependencies> {
    let names: Vec<String> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect();

    if names.is_empty() {
        None
    } else {
        Some(Dependencies { names })
    }
}

/// Parse sandbox file: key=value overrides of the strict default.
fn parse_sandbox(content: &str, name: &str) -> SandboxConfig {
    let mut cfg = SandboxConfig::strict_default();

    for (key, val) in parse_kv(content) {
        match key {
            "read" => cfg.read_paths = parse_path_list(val),
            "write" => cfg.write_paths = parse_path_list(val),
            "exec" => cfg.exec_paths = parse_path_list(val),
            "bind" => cfg.bind_ports = parse_port_list(val),
            "connect" => cfg.connect_ports = parse_port_list(val),
            "pid-namespace" => {
                if let Some(b) = parse_bool(val) {
                    cfg.pid_namespace = b;
                }
            }
            "mount-namespace" => {
                if let Some(b) = parse_bool(val) {
                    cfg.mount_namespace = b;
                }
            }
            "ipc-namespace" => {
                if let Some(b) = parse_bool(val) {
                    cfg.ipc_namespace = b;
                }
            }
            "uts-namespace" => {
                if let Some(b) = parse_bool(val) {
                    cfg.uts_namespace = b;
                }
            }
            "network-namespace" => match val {
                "private" => cfg.network_namespace = NetworkNamespace::Private,
                "host" => cfg.network_namespace = NetworkNamespace::Host,
                _ => eprintln!("arkhd: config [{name}]: unknown network-namespace '{val}'"),
            },
            "private-tmp" => {
                if let Some(b) = parse_bool(val) {
                    cfg.private_tmp = b;
                }
            }
            "read-only-root" => {
                if let Some(b) = parse_bool(val) {
                    cfg.read_only_root = b;
                }
            }
            "caps" => cfg.capabilities = parse_cap_list(val),
            "seccomp" => match val {
                "default" => cfg.seccomp_profile = SeccompProfile::Default,
                "disabled" => cfg.seccomp_profile = SeccompProfile::Disabled,
                _ => eprintln!("arkhd: config [{name}]: unknown seccomp profile '{val}'"),
            },
            "ipc-scope" => match val {
                "scoped" => cfg.ipc_scope = IpcScope::Scoped,
                "unscoped" => cfg.ipc_scope = IpcScope::Unscoped,
                _ => eprintln!("arkhd: config [{name}]: unknown ipc-scope '{val}'"),
            },
            "sandbox" if val == "permissive" => cfg.permissive = true,
            _ => eprintln!("arkhd: config [{name}]: unknown sandbox key '{key}'"),
        }
    }

    cfg
}

/// Parse resources file: key=value cgroup limits.
fn parse_resources(content: &str, name: &str) -> Option<ResourceLimits> {
    let mut limits = ResourceLimits {
        memory_max: None,
        memory_high: None,
        cpu_weight: None,
        cpu_max: None,
        io_weight: None,
        pids_max: None,
    };
    let mut has_any = false;

    for (key, val) in parse_kv(content) {
        match key {
            "memory-max" => {
                if let Some(v) = parse_size(val) {
                    limits.memory_max = Some(v);
                    has_any = true;
                }
            }
            "memory-high" => {
                if let Some(v) = parse_size(val) {
                    limits.memory_high = Some(v);
                    has_any = true;
                }
            }
            "cpu-weight" => {
                if let Ok(v) = val.parse::<u32>() {
                    limits.cpu_weight = Some(v);
                    has_any = true;
                }
            }
            "cpu-max" => {
                // Format: "quota_us period_us" (space-separated)
                let parts: Vec<&str> = val.split_whitespace().collect();
                if parts.len() == 2 {
                    if let (Ok(q), Ok(p)) = (parts[0].parse::<u64>(), parts[1].parse::<u64>()) {
                        limits.cpu_max = Some((q, p));
                        has_any = true;
                    }
                } else {
                    eprintln!("arkhd: config [{name}]: invalid cpu-max format '{val}'");
                }
            }
            "io-weight" => {
                if let Ok(v) = val.parse::<u32>() {
                    limits.io_weight = Some(v);
                    has_any = true;
                }
            }
            "pids-max" => {
                if let Ok(v) = val.parse::<u32>() {
                    limits.pids_max = Some(v);
                    has_any = true;
                }
            }
            _ => eprintln!("arkhd: config [{name}]: unknown resources key '{key}'"),
        }
    }

    if has_any {
        Some(limits)
    } else {
        None
    }
}

/// Parse listen file: one socket spec per line (tcp:PORT, tcp6:ADDR, unix:PATH).
fn parse_listen(content: &str, name: &str) -> Option<ListenSockets> {
    let sockets: Vec<ListenAddr> = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            if let Some(port_str) = l.strip_prefix("tcp:") {
                port_str.parse::<u16>().ok().map(ListenAddr::Tcp)
            } else if let Some(addr_str) = l.strip_prefix("tcp6:") {
                addr_str.parse().ok().map(ListenAddr::Tcp6)
            } else if let Some(path) = l.strip_prefix("unix:") {
                Some(ListenAddr::Unix(PathBuf::from(path)))
            } else {
                eprintln!("arkhd: config [{name}]: unknown listen format '{l}'");
                None
            }
        })
        .collect();

    if sockets.is_empty() {
        None
    } else {
        Some(ListenSockets { sockets })
    }
}

/// Parse ready file: mode and timeout key=value pairs.
fn parse_ready(content: &str, name: &str) -> (ReadinessMode, Duration) {
    let mut mode = ReadinessMode::File;
    let mut timeout = Duration::from_secs(30);

    for (key, val) in parse_kv(content) {
        match key {
            "mode" => match val {
                "file" => mode = ReadinessMode::File,
                "fd" => mode = ReadinessMode::Fd,
                "timeout" => mode = ReadinessMode::Timeout,
                _ => eprintln!("arkhd: config [{name}]: unknown readiness mode '{val}'"),
            },
            "timeout" => {
                if let Ok(secs) = val.parse::<u64>() {
                    timeout = Duration::from_secs(secs);
                }
            }
            _ => eprintln!("arkhd: config [{name}]: unknown ready key '{key}'"),
        }
    }

    (mode, timeout)
}

/// Parse log/config file: max-size and max-files.
fn parse_log_config(content: &str, name: &str) -> LogConfig {
    let mut cfg = LogConfig::default();

    for (key, val) in parse_kv(content) {
        match key {
            "max-size" => {
                if let Some(v) = parse_size(val) {
                    cfg.max_size = v;
                }
            }
            "max-files" => {
                if let Ok(v) = val.parse::<u32>() {
                    cfg.max_files = v;
                }
            }
            _ => eprintln!("arkhd: config [{name}]: unknown log key '{key}'"),
        }
    }

    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;
    use std::fs;

    /// Create a temp dir whose leaf component is the service name.
    /// load_service extracts the name from the leaf, so this matters.
    fn test_dir(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("arkhe-cfgtest-{}", std::process::id()));
        let sv_dir = base.join(name);
        let _ = fs::remove_dir_all(&sv_dir);
        fs::create_dir_all(&sv_dir).unwrap();
        sv_dir
    }

    fn make_run(dir: &Path) {
        fs::write(dir.join("run"), "#!/bin/sh\nexec sleep infinity\n").unwrap();
    }

    #[test]
    fn minimal_service() {
        let dir = test_dir("minimal");
        make_run(&dir);

        let mut world = World::new();
        let name = load_service(&mut world, &dir).unwrap();

        assert_eq!(name, "minimal");
        assert_eq!(world.len(), 1);
        assert_eq!(world.names[0], "minimal");
        assert!(world.run_configs[0].run_path.ends_with("run"));
        assert!(world.run_configs[0].finish_path.is_none());
        assert!(world.dependencies[0].is_none());
        assert!(world.resource_limits[0].is_none());
        assert!(world.listen_sockets[0].is_none());
        // Sandbox defaults
        assert!(world.sandbox_configs[0].pid_namespace);
        assert!(world.sandbox_configs[0].read_paths.is_empty());
        // Readiness defaults
        assert_eq!(world.readiness[0].mode, ReadinessMode::File);
        assert!(!world.readiness[0].satisfied);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_run_file() {
        let dir = test_dir("no-run");

        let mut world = World::new();
        assert!(load_service(&mut world, &dir).is_err());
        assert_eq!(world.len(), 0);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn finish_file() {
        let dir = test_dir("finish");
        make_run(&dir);
        fs::write(dir.join("finish"), "#!/bin/sh\nexit 0\n").unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        assert!(world.run_configs[0].finish_path.is_some());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn depends_parsing() {
        let dir = test_dir("deps");
        make_run(&dir);
        fs::write(
            dir.join("depends"),
            "network-online\ndns-ready\n# comment\n\ntls-certs\n",
        )
        .unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        let deps = world.dependencies[0].as_ref().unwrap();
        assert_eq!(deps.names, vec!["network-online", "dns-ready", "tls-certs"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_parsing() {
        let dir = test_dir("sandbox");
        make_run(&dir);
        fs::write(
            dir.join("sandbox"),
            "read = /usr, /lib, /etc/nginx\n\
             write = /var/log/nginx\n\
             exec = /usr/sbin/nginx\n\
             bind = 80, 443\n\
             connect = none\n\
             network-namespace = host\n\
             private-tmp = no\n\
             caps = net_bind_service, net_admin\n",
        )
        .unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        let sb = &world.sandbox_configs[0];
        assert_eq!(sb.read_paths.len(), 3);
        assert_eq!(sb.read_paths[0], PathBuf::from("/usr"));
        assert_eq!(sb.read_paths[1], PathBuf::from("/lib"));
        assert_eq!(sb.read_paths[2], PathBuf::from("/etc/nginx"));
        assert_eq!(sb.write_paths, vec![PathBuf::from("/var/log/nginx")]);
        assert_eq!(sb.exec_paths, vec![PathBuf::from("/usr/sbin/nginx")]);
        assert_eq!(sb.bind_ports, vec![80, 443]);
        assert!(sb.connect_ports.is_empty());
        assert_eq!(sb.network_namespace, NetworkNamespace::Host);
        assert!(!sb.private_tmp);
        assert_eq!(sb.capabilities, vec!["net_bind_service", "net_admin"]);
        // Unspecified keys keep defaults
        assert!(sb.pid_namespace);
        assert!(sb.mount_namespace);
        assert!(sb.read_only_root);
        assert!(!sb.permissive);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn sandbox_permissive() {
        let dir = test_dir("permissive");
        make_run(&dir);
        fs::write(dir.join("sandbox"), "sandbox = permissive\n").unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        assert!(world.sandbox_configs[0].permissive);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn resources_parsing() {
        let dir = test_dir("resources");
        make_run(&dir);
        fs::write(
            dir.join("resources"),
            "memory-max = 512M\n\
             memory-high = 384M\n\
             cpu-weight = 100\n\
             cpu-max = 80000 100000\n\
             io-weight = 100\n\
             pids-max = 64\n",
        )
        .unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        let rl = world.resource_limits[0].as_ref().unwrap();
        assert_eq!(rl.memory_max, Some(512 * 1024 * 1024));
        assert_eq!(rl.memory_high, Some(384 * 1024 * 1024));
        assert_eq!(rl.cpu_weight, Some(100));
        assert_eq!(rl.cpu_max, Some((80000, 100000)));
        assert_eq!(rl.io_weight, Some(100));
        assert_eq!(rl.pids_max, Some(64));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn listen_parsing() {
        let dir = test_dir("listen");
        make_run(&dir);
        fs::write(
            dir.join("listen"),
            "tcp:80\ntcp:443\ntcp6:[::]:8080\nunix:/run/app.sock\n",
        )
        .unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        let ls = world.listen_sockets[0].as_ref().unwrap();
        assert_eq!(ls.sockets.len(), 4);
        match &ls.sockets[0] {
            ListenAddr::Tcp(port) => assert_eq!(*port, 80),
            _ => panic!("expected Tcp"),
        }
        match &ls.sockets[1] {
            ListenAddr::Tcp(port) => assert_eq!(*port, 443),
            _ => panic!("expected Tcp"),
        }
        match &ls.sockets[2] {
            ListenAddr::Tcp6(addr) => assert_eq!(addr.port(), 8080),
            _ => panic!("expected Tcp6"),
        }
        match &ls.sockets[3] {
            ListenAddr::Unix(path) => assert_eq!(path, &PathBuf::from("/run/app.sock")),
            _ => panic!("expected Unix"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ready_parsing() {
        let dir = test_dir("ready");
        make_run(&dir);
        fs::write(dir.join("ready"), "mode = timeout\ntimeout = 10\n").unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        assert_eq!(world.readiness[0].mode, ReadinessMode::Timeout);
        assert_eq!(world.readiness[0].timeout, Duration::from_secs(10));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn log_config_parsing() {
        let dir = test_dir("log-config");
        make_run(&dir);
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(dir.join("log/config"), "max-size = 5M\nmax-files = 20\n").unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        assert_eq!(world.log_configs[0].max_size, 5 * 1024 * 1024);
        assert_eq!(world.log_configs[0].max_files, 20);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_optional_files() {
        let dir = test_dir("empty-files");
        make_run(&dir);
        fs::write(dir.join("depends"), "").unwrap();
        fs::write(dir.join("sandbox"), "").unwrap();
        fs::write(dir.join("resources"), "").unwrap();
        fs::write(dir.join("listen"), "").unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        assert!(world.dependencies[0].is_none());
        assert!(world.resource_limits[0].is_none());
        assert!(world.listen_sockets[0].is_none());
        // Empty sandbox → strict defaults (not changed)
        assert!(world.sandbox_configs[0].pid_namespace);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_values_skipped() {
        let dir = test_dir("malformed");
        make_run(&dir);
        fs::write(
            dir.join("resources"),
            "memory-max = not_a_number\ncpu-weight = abc\npids-max = 64\n",
        )
        .unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        let rl = world.resource_limits[0].as_ref().unwrap();
        assert!(rl.memory_max.is_none());
        assert!(rl.cpu_weight.is_none());
        assert_eq!(rl.pids_max, Some(64));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn comments_and_whitespace() {
        let dir = test_dir("comments");
        make_run(&dir);
        fs::write(
            dir.join("depends"),
            "# This is a comment\n  network-online  \ndns-ready\n  \n",
        )
        .unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        let deps = world.dependencies[0].as_ref().unwrap();
        assert_eq!(deps.names, vec!["network-online", "dns-ready"]);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_suffixes() {
        assert_eq!(parse_size("100"), Some(100));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1M"), Some(1_048_576));
        assert_eq!(parse_size("1G"), Some(1_073_741_824));
        assert_eq!(parse_size("512M"), Some(536_870_912));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn full_service_all_files() {
        let dir = test_dir("full");
        make_run(&dir);
        fs::write(dir.join("finish"), "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(dir.join("depends"), "network-online\n").unwrap();
        fs::write(
            dir.join("sandbox"),
            "read = /usr, /etc/app\nbind = 8080\nnetwork-namespace = host\n",
        )
        .unwrap();
        fs::write(dir.join("resources"), "memory-max = 256M\npids-max = 32\n").unwrap();
        fs::write(dir.join("listen"), "tcp:8080\n").unwrap();
        fs::write(dir.join("ready"), "mode = fd\ntimeout = 15\n").unwrap();
        fs::create_dir_all(dir.join("log")).unwrap();
        fs::write(dir.join("log/config"), "max-size = 2M\nmax-files = 5\n").unwrap();

        let mut world = World::new();
        load_service(&mut world, &dir).unwrap();

        assert_eq!(world.len(), 1);
        assert!(world.run_configs[0].finish_path.is_some());
        assert_eq!(
            world.dependencies[0].as_ref().unwrap().names,
            vec!["network-online"]
        );
        assert_eq!(world.sandbox_configs[0].read_paths.len(), 2);
        assert_eq!(world.sandbox_configs[0].bind_ports, vec![8080]);
        assert_eq!(
            world.sandbox_configs[0].network_namespace,
            NetworkNamespace::Host
        );
        assert_eq!(
            world.resource_limits[0].as_ref().unwrap().memory_max,
            Some(256 * 1024 * 1024)
        );
        assert_eq!(
            world.resource_limits[0].as_ref().unwrap().pids_max,
            Some(32)
        );
        assert_eq!(world.listen_sockets[0].as_ref().unwrap().sockets.len(), 1);
        assert_eq!(world.readiness[0].mode, ReadinessMode::Fd);
        assert_eq!(world.readiness[0].timeout, Duration::from_secs(15));
        assert_eq!(world.log_configs[0].max_size, 2 * 1024 * 1024);
        assert_eq!(world.log_configs[0].max_files, 5);

        let _ = fs::remove_dir_all(&dir);
    }
}
