//! Sandbox module — apply Landlock + seccomp + capability restrictions.
//!
//! Called in the child process after clone3, before exec.
//! Every service is sandboxed by default. No unsandboxed mode.
//! The "permissive" flag is an escape hatch that is logged and visible in `ark check`.
//!
//! Layers applied in order:
//! 1. Landlock filesystem + network rules (default-deny, allowlist)
//! 2. Seccomp syscall filter (deny-list of dangerous syscalls)
//! 3. Capability dropping (keep only what's declared)
//!
//! Zero unsafe in this file. All syscalls go through sys.rs.

use std::path::Path;

use crate::components::{IpcScope, SandboxConfig, SeccompProfile};
use crate::sys;

/// Maximum filesystem access rights for a given Landlock ABI version.
/// ABI 0: no Landlock. ABI 1: bits 0-12. ABI 2: +REFER. ABI 3: +TRUNCATE.
/// ABI 4-5: same as 3. ABI 6+: +IOCTL_DEV.
pub fn max_fs_access(abi: u32) -> u64 {
    match abi {
        0 => 0,
        1 => (1 << 13) - 1,     // bits 0-12
        2 => (1 << 14) - 1,     // + REFER
        3..=5 => (1 << 15) - 1, // + TRUNCATE
        _ => (1 << 16) - 1,     // + IOCTL_DEV (ABI 6+)
    }
}

/// Maximum network access rights for a given Landlock ABI version.
/// Network rules require ABI 4+.
pub fn max_net_access(abi: u32) -> u64 {
    match abi {
        0..=3 => 0,
        _ => (1 << 2) - 1, // BIND_TCP | CONNECT_TCP
    }
}

/// Maximum IPC scoping flags for a given Landlock ABI version.
/// Scoping requires ABI 5+.
pub fn max_scoped(abi: u32) -> u64 {
    match abi {
        0..=4 => 0,
        _ => (1 << 2) - 1, // ABSTRACT_UNIX | SIGNAL
    }
}

/// Apply the full sandbox to the current process.
///
/// Called in the child after clone3, before exec. If something fails,
/// it is logged to stderr (which goes to the service's log pipe) and
/// the child continues with degraded sandboxing. A completely failed
/// sandbox is better than a service that won't start.
pub fn apply_sandbox(config: &SandboxConfig, run_script: &Path, landlock_abi: u32) {
    if config.permissive {
        eprintln!("arkhd: sandbox: WARNING — permissive mode, no sandbox applied");
        return;
    }

    // 1. Landlock
    apply_landlock(config, run_script, landlock_abi);

    // 2. Seccomp
    if config.seccomp_profile != SeccompProfile::Disabled {
        if let Err(e) = sys::apply_seccomp_default() {
            eprintln!("arkhd: sandbox: seccomp failed: {e}");
        }
    }

    // 3. Capabilities
    if let Err(e) = sys::drop_capabilities(&config.capabilities) {
        eprintln!("arkhd: sandbox: cap drop failed: {e}");
    }
}

/// Apply Landlock filesystem and network rules, masked to the detected ABI.
fn apply_landlock(config: &SandboxConfig, run_script: &Path, abi: u32) {
    if abi == 0 {
        eprintln!("arkhd: sandbox: Landlock not available (ABI 0), skipping filesystem sandbox");
        return;
    }

    let fs_mask = max_fs_access(abi);
    let net_mask = max_net_access(abi);
    let scope_mask = max_scoped(abi);

    // Determine scoped flags, masked to ABI
    let scoped = match config.ipc_scope {
        IpcScope::Scoped => {
            let s = (sys::LL_SCOPE_ABSTRACT_UNIX | sys::LL_SCOPE_SIGNAL) & scope_mask;
            if s == 0 {
                eprintln!("arkhd: sandbox: Landlock ABI {abi} < 5: IPC scoping not enforced");
            }
            s
        }
        IpcScope::Unscoped => 0,
    };

    // Mask handled access to what the kernel supports
    let handled_fs = sys::LL_FS_ALL & fs_mask;
    let handled_net = sys::LL_NET_ALL & net_mask;

    if handled_net == 0 && (!config.bind_ports.is_empty() || !config.connect_ports.is_empty()) {
        eprintln!("arkhd: sandbox: Landlock ABI {abi} < 4: network rules not enforced");
    }

    // Create ruleset with masked access types
    let ruleset = match sys::landlock_create_ruleset(handled_fs, handled_net, scoped) {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("arkhd: sandbox: landlock create_ruleset failed: {e}");
            return;
        }
    };

    // Auto-add the run script's directory with read + exec
    if let Some(parent) = run_script.parent() {
        let _ = sys::landlock_add_rule_path(
            &ruleset,
            parent,
            (sys::LL_FS_READ_FILE | sys::LL_FS_READ_DIR | sys::LL_FS_EXECUTE) & fs_mask,
        );
    }

    // Read paths → read access (file + dir)
    let read_access = (sys::LL_FS_READ_FILE | sys::LL_FS_READ_DIR) & fs_mask;
    for path in &config.read_paths {
        if let Err(e) = sys::landlock_add_rule_path(&ruleset, path, read_access) {
            eprintln!(
                "arkhd: sandbox: landlock read rule for {}: {e}",
                path.display()
            );
        }
    }

    // Write paths → read + write + create + remove access
    let write_access = (sys::LL_FS_READ_FILE
        | sys::LL_FS_READ_DIR
        | sys::LL_FS_WRITE_FILE
        | sys::LL_FS_REMOVE_FILE
        | sys::LL_FS_REMOVE_DIR
        | sys::LL_FS_MAKE_REG
        | sys::LL_FS_MAKE_DIR
        | sys::LL_FS_MAKE_SYM
        | sys::LL_FS_TRUNCATE)
        & fs_mask;
    for path in &config.write_paths {
        if let Err(e) = sys::landlock_add_rule_path(&ruleset, path, write_access) {
            eprintln!(
                "arkhd: sandbox: landlock write rule for {}: {e}",
                path.display()
            );
        }
    }

    // Exec paths → read + execute access
    let exec_access = (sys::LL_FS_READ_FILE | sys::LL_FS_READ_DIR | sys::LL_FS_EXECUTE) & fs_mask;
    for path in &config.exec_paths {
        if let Err(e) = sys::landlock_add_rule_path(&ruleset, path, exec_access) {
            eprintln!(
                "arkhd: sandbox: landlock exec rule for {}: {e}",
                path.display()
            );
        }
    }

    // Network: bind ports (only if ABI supports it)
    if net_mask > 0 {
        for &port in &config.bind_ports {
            if let Err(e) =
                sys::landlock_add_rule_net(&ruleset, port, sys::LL_NET_BIND_TCP & net_mask)
            {
                eprintln!("arkhd: sandbox: landlock bind rule for port {port}: {e}");
            }
        }

        // Network: connect ports
        for &port in &config.connect_ports {
            if let Err(e) =
                sys::landlock_add_rule_net(&ruleset, port, sys::LL_NET_CONNECT_TCP & net_mask)
            {
                eprintln!("arkhd: sandbox: landlock connect rule for port {port}: {e}");
            }
        }
    }

    // Enforce — this is the point of no return
    if let Err(e) = sys::landlock_restrict_self(&ruleset) {
        eprintln!("arkhd: sandbox: landlock restrict_self failed: {e}");
    }
    // ruleset fd is dropped here (closed by OwnedFd), then close_range cleans up any stragglers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::SandboxConfig;

    #[test]
    fn permissive_returns_immediately() {
        let mut config = SandboxConfig::strict_default();
        config.permissive = true;
        // Should not panic or fail — just logs a warning
        apply_sandbox(&config, std::path::Path::new("/etc/sv/test/run"), 6);
    }

    #[test]
    fn abi_detection() {
        // landlock_abi_version should return 0 on non-Linux or a valid ABI
        let abi = sys::landlock_abi_version();
        // On any platform, this should not panic
        assert!(abi <= 255, "ABI version suspiciously high: {abi}");
    }

    #[test]
    fn max_fs_access_by_abi() {
        assert_eq!(max_fs_access(0), 0);
        assert_eq!(max_fs_access(1), (1 << 13) - 1); // bits 0-12
        assert_eq!(max_fs_access(2), (1 << 14) - 1); // + REFER
        assert_eq!(max_fs_access(3), (1 << 15) - 1); // + TRUNCATE
        assert_eq!(max_fs_access(4), (1 << 15) - 1); // same as 3
        assert_eq!(max_fs_access(5), (1 << 15) - 1); // same as 3
        assert_eq!(max_fs_access(6), (1 << 16) - 1); // + IOCTL_DEV
    }

    #[test]
    fn max_net_access_by_abi() {
        assert_eq!(max_net_access(0), 0);
        assert_eq!(max_net_access(1), 0);
        assert_eq!(max_net_access(2), 0);
        assert_eq!(max_net_access(3), 0);
        assert_eq!(max_net_access(4), (1 << 2) - 1); // BIND_TCP | CONNECT_TCP
        assert_eq!(max_net_access(5), (1 << 2) - 1);
        assert_eq!(max_net_access(6), (1 << 2) - 1);
    }

    #[test]
    fn max_scoped_by_abi() {
        assert_eq!(max_scoped(0), 0);
        assert_eq!(max_scoped(1), 0);
        assert_eq!(max_scoped(4), 0);
        assert_eq!(max_scoped(5), (1 << 2) - 1); // ABSTRACT_UNIX | SIGNAL
        assert_eq!(max_scoped(6), (1 << 2) - 1);
    }

    // Integration tests require root + Landlock kernel support.
    // Run with: sudo cargo test -- sandbox
    #[test]
    fn landlock_ruleset_creation() {
        // Try creating a ruleset — skip if kernel doesn't support Landlock
        match sys::landlock_create_ruleset(sys::LL_FS_ALL, sys::LL_NET_ALL, 0) {
            Ok(_fd) => { /* Landlock is available */ }
            Err(_) => {
                eprintln!("landlock_ruleset_creation: skipped (Landlock not available)");
            }
        }
    }

    #[test]
    fn cap_name_mapping() {
        assert_eq!(sys::cap_name_to_number("net_bind_service"), Some(10));
        assert_eq!(sys::cap_name_to_number("cap_net_admin"), Some(12));
        assert_eq!(sys::cap_name_to_number("sys_admin"), Some(21));
        assert_eq!(sys::cap_name_to_number("nonexistent"), None);
    }

    #[test]
    fn landlock_rule_on_regular_file() {
        // Landlock requires directory fds. landlock_add_rule_path should
        // automatically use the parent directory when given a regular file.
        let ruleset = match sys::landlock_create_ruleset(sys::LL_FS_ALL, sys::LL_NET_ALL, 0) {
            Ok(fd) => fd,
            Err(_) => {
                eprintln!("landlock_rule_on_regular_file: skipped (Landlock not available)");
                return;
            }
        };

        // /etc/hostname is a regular file on most Linux systems
        let path = std::path::Path::new("/etc/hostname");
        if !path.exists() {
            eprintln!("landlock_rule_on_regular_file: skipped (/etc/hostname not found)");
            return;
        }

        // This should NOT return an error — it should silently use /etc/ instead
        let result = sys::landlock_add_rule_path(&ruleset, path, sys::LL_FS_READ_FILE);
        assert!(result.is_ok(), "expected Ok, got: {:?}", result.err());
    }
}
