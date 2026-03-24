//! Mount namespace setup — called in child after clone3, before sandbox.
//!
//! When a service has mount_namespace = true (the default):
//! 1. Private mount propagation — prevent mount events leaking to host
//! 2. Read-only root if read_only_root = true — remount / as MS_RDONLY
//! 3. Private /tmp if private_tmp = true — mount tmpfs on /tmp
//!
//! ID-mapped mounts for write paths are plumbed (syscall wrappers in sys.rs)
//! but not yet wired. They require creating a user namespace with specific
//! UID/GID mappings — follow-up work.
//!
//! Zero unsafe in this file.

use std::path::Path;

use crate::components::SandboxConfig;
use crate::sys;

/// Set up the mount namespace for a sandboxed service.
/// Called in the child process after clone3, before sandbox application.
pub fn setup_mount_namespace(config: &SandboxConfig) {
    if !config.mount_namespace || config.permissive {
        return;
    }

    // 1. Private mount propagation — isolate from host
    if let Err(e) = sys::make_private_propagation() {
        eprintln!("arkhd: mounts: private propagation: {e}");
    }

    // 2. Read-only root filesystem
    if config.read_only_root {
        if let Err(e) = sys::remount_readonly_root() {
            eprintln!("arkhd: mounts: read-only root: {e}");
        }
    }

    // 3. Private /tmp (fresh tmpfs per service)
    if config.private_tmp {
        if let Err(e) = sys::mount_tmpfs(Path::new("/tmp")) {
            eprintln!("arkhd: mounts: private /tmp: {e}");
        }
    }

    // TODO: ID-mapped mounts for write paths.
    // The syscall wrappers (sys::open_tree, sys::mount_setattr_idmap,
    // sys::move_mount) are ready. Wiring requires:
    //   1. Create a user namespace with the desired UID/GID mapping
    //   2. For each write_path: open_tree → mount_setattr_idmap → move_mount
    // This enables non-root services to write to root-owned host paths.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_mount_namespace_returns_immediately() {
        let mut config = SandboxConfig::strict_default();
        config.mount_namespace = false;
        // Should not panic — just returns
        setup_mount_namespace(&config);
    }

    #[test]
    fn mount_operations_fail_gracefully_without_root() {
        // Without root/CAP_SYS_ADMIN, mount operations fail.
        // setup_mount_namespace logs errors and continues.
        let config = SandboxConfig::strict_default();
        // This will log errors but not panic
        setup_mount_namespace(&config);
    }

    #[test]
    fn open_tree_returns_error_without_privileges() {
        // open_tree requires privileges; verify it returns a sensible error
        let result = sys::open_tree(Path::new("/tmp"));
        // Either succeeds (root) or fails with EPERM/ENOSYS
        if let Err(e) = result {
            assert!(
                e.raw_os_error() == Some(libc::EPERM)
                    || e.raw_os_error() == Some(libc::ENOSYS)
                    || e.raw_os_error() == Some(libc::ENOENT),
                "unexpected error: {e}"
            );
        }
    }
}
