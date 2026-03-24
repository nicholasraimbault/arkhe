//! Unsafe boundary — ALL unsafe in the supervisor lives in this file.
//!
//! Every other .rs file in the supervisor contains zero unsafe blocks.
//! This module exports safe wrapper functions for: signalfd, io_uring SQE push,
//! pipe creation, process spawning (clone3), fanotify, landlock, seccomp, and
//! capability dropping.

use std::ffi::{CStr, CString};
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use io_uring::IoUring;

use crate::components::SandboxConfig;
use crate::error::SupervisorError;

// ═══════════════════════════════════════════════════════════════════════════════
// Signal handling
// ═══════════════════════════════════════════════════════════════════════════════

/// Decoded signal information from signalfd.
pub struct SignalInfo {
    pub signo: i32,
    pub pid: u32,
    #[allow(dead_code)]
    pub status: i32,
}

/// Block SIGCHLD, SIGTERM, and SIGHUP, then create a signalfd for them.
pub fn setup_signals() -> Result<OwnedFd, SupervisorError> {
    let mut mask: libc::sigset_t = unsafe { MaybeUninit::zeroed().assume_init() };
    unsafe {
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGCHLD);
        libc::sigaddset(&mut mask, libc::SIGTERM);
        libc::sigaddset(&mut mask, libc::SIGHUP);
    }
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) } < 0 {
        return Err(SupervisorError::SignalSetup(io::Error::last_os_error()));
    }
    let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
    if fd < 0 {
        return Err(SupervisorError::SignalSetup(io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read one signalfd_siginfo. Returns None on EAGAIN.
pub fn read_signal(fd: &OwnedFd) -> Result<Option<SignalInfo>, SupervisorError> {
    let mut info = MaybeUninit::<libc::signalfd_siginfo>::uninit();
    let n = unsafe {
        libc::read(
            fd.as_raw_fd(),
            info.as_mut_ptr().cast(),
            std::mem::size_of::<libc::signalfd_siginfo>(),
        )
    };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(None);
        }
        return Err(SupervisorError::SignalRead(err));
    }
    let info = unsafe { info.assume_init() };
    Ok(Some(SignalInfo {
        signo: info.ssi_signo as i32,
        pid: info.ssi_pid,
        status: info.ssi_status as i32,
    }))
}

// ═══════════════════════════════════════════════════════════════════════════════
// io_uring
// ═══════════════════════════════════════════════════════════════════════════════

/// Push an SQE onto the io_uring submission queue.
pub fn push_sqe(
    ring: &mut IoUring,
    entry: &io_uring::squeue::Entry,
) -> Result<(), SupervisorError> {
    unsafe { ring.submission().push(entry) }
        .map_err(|_| SupervisorError::RingSubmit(io::Error::from(io::ErrorKind::Other)))
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pipe creation
// ═══════════════════════════════════════════════════════════════════════════════

/// Create a pipe with O_CLOEXEC. Returns (read_end, write_end).
pub fn create_pipe() -> Result<(OwnedFd, OwnedFd), SupervisorError> {
    let mut fds = [0i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
        return Err(SupervisorError::PipeCreate(io::Error::last_os_error()));
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

// ═══════════════════════════════════════════════════════════════════════════════
// Process spawning via clone3
// ═══════════════════════════════════════════════════════════════════════════════

pub const CLONE_PIDFD: u64 = 0x0000_1000;
pub const CLONE_NEWPID: u64 = 0x2000_0000;
pub const CLONE_NEWNS: u64 = 0x0002_0000;
pub const CLONE_NEWIPC: u64 = 0x0800_0000;
pub const CLONE_INTO_CGROUP: u64 = 0x0000_0002_0000_0000;
const CLOSE_RANGE_UNSHARE: u32 = 2;

pub struct SpawnResult {
    pub pid: u32,
    pub pidfd: OwnedFd,
}

#[repr(C)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// Fork via clone3, apply sandbox in child, then exec.
/// Returns SpawnResult in parent only. Child never returns.
///
/// `listen_fds` are raw fds of pre-bound sockets for socket activation.
/// In the child, they are dup2'd to fd 3, 4, 5... before close_range.
pub fn clone3_exec(
    clone_flags: u64,
    cgroup_fd: RawFd,
    log_write_fd: RawFd,
    run_path: &CStr,
    envp: &[CString],
    sandbox_config: &SandboxConfig,
    listen_fds: &[RawFd],
    landlock_abi: u32,
) -> Result<SpawnResult, SupervisorError> {
    let argv: [*const libc::c_char; 2] = [run_path.as_ptr(), std::ptr::null()];
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|s| s.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    let mut pidfd_out: libc::c_int = -1;
    let args = CloneArgs {
        flags: clone_flags,
        pidfd: &mut pidfd_out as *mut libc::c_int as u64,
        child_tid: 0,
        parent_tid: 0,
        exit_signal: libc::SIGCHLD as u64,
        stack: 0,
        stack_size: 0,
        tls: 0,
        set_tid: 0,
        set_tid_size: 0,
        cgroup: cgroup_fd as u64,
    };

    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone3,
            &args as *const CloneArgs,
            std::mem::size_of::<CloneArgs>(),
        )
    };
    if ret < 0 {
        return Err(SupervisorError::SpawnFork(
            String::new(),
            io::Error::last_os_error(),
        ));
    }

    if ret == 0 {
        // CHILD — single-threaded, COW pages, Rust allocation is safe.
        unsafe {
            libc::dup2(log_write_fd, libc::STDOUT_FILENO);
            libc::dup2(log_write_fd, libc::STDERR_FILENO);

            // Socket activation: dup2 listen fds to 3, 4, 5, ...
            for (i, &fd) in listen_fds.iter().enumerate() {
                libc::dup2(fd, 3 + i as libc::c_int);
            }

            // Mount namespace setup — private propagation, ro root, private /tmp
            crate::systems::mounts::setup_mount_namespace(sandbox_config);

            // Apply sandbox (Landlock + seccomp + caps) — needs to open path fds
            let run_path_for_sandbox = Path::new(std::ffi::OsStr::from_bytes(run_path.to_bytes()));
            crate::sandbox::apply_sandbox(sandbox_config, run_path_for_sandbox, landlock_abi);

            // Close all fds above the socket-activated range
            let close_start = 3u32 + listen_fds.len() as u32;
            libc::syscall(
                libc::SYS_close_range,
                close_start,
                u32::MAX,
                CLOSE_RANGE_UNSHARE,
            );
            libc::execve(run_path.as_ptr(), argv.as_ptr(), envp_ptrs.as_ptr());
            libc::_exit(127);
        }
    }

    Ok(SpawnResult {
        pid: ret as u32,
        pidfd: unsafe { OwnedFd::from_raw_fd(pidfd_out) },
    })
}

// ═══════════════════════════════════════════════════════════════════════════════
// Fanotify — filesystem event monitoring
// ═══════════════════════════════════════════════════════════════════════════════

// fanotify_init flags
#[allow(dead_code)]
const FAN_CLASS_NOTIF: libc::c_uint = 0x0000_0000;
#[allow(dead_code)]
const FAN_NONBLOCK: libc::c_uint = 0x0000_0002;
#[allow(dead_code)]
const FAN_REPORT_DFID_NAME: libc::c_uint = 0x0000_0C00;
// fanotify_mark flags
#[allow(dead_code)]
const FAN_MARK_ADD: libc::c_uint = 0x0000_0001;
// fanotify event masks
pub const FAN_CREATE: u64 = 0x0000_0100;
pub const FAN_DELETE: u64 = 0x0000_0200;
pub const FAN_ONDIR: u64 = 0x4000_0000;
// fanotify info types
const FAN_EVENT_INFO_TYPE_DFID_NAME: u8 = 2;

/// A parsed fanotify event with the child name and event mask.
pub struct FanotifyEvent {
    pub mask: u64,
    pub name: String,
}

/// Set up fanotify watching /run/ready/ and /etc/sv/.
/// FAN_CREATE | FAN_DELETE on ready dir, plus FAN_ONDIR on sv dir.
#[allow(dead_code)]
pub fn setup_fanotify() -> Result<OwnedFd, SupervisorError> {
    let fd = unsafe {
        libc::syscall(
            libc::SYS_fanotify_init,
            FAN_CLASS_NOTIF | FAN_NONBLOCK | FAN_REPORT_DFID_NAME,
            libc::O_RDONLY as libc::c_uint | libc::O_CLOEXEC as libc::c_uint,
        ) as libc::c_int
    };
    if fd < 0 {
        return Err(SupervisorError::FanotifySetup(io::Error::last_os_error()));
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // Mark /run/ready/ for file create/delete
    let ready = CString::new("/run/ready").unwrap();
    if unsafe {
        libc::syscall(
            libc::SYS_fanotify_mark,
            owned.as_raw_fd(),
            FAN_MARK_ADD,
            FAN_CREATE | FAN_DELETE,
            libc::AT_FDCWD,
            ready.as_ptr(),
        )
    } < 0
    {
        return Err(SupervisorError::FanotifySetup(io::Error::last_os_error()));
    }

    // Mark /etc/sv/ for directory create/delete (drop-to-activate)
    let sv = CString::new("/etc/sv").unwrap();
    if unsafe {
        libc::syscall(
            libc::SYS_fanotify_mark,
            owned.as_raw_fd(),
            FAN_MARK_ADD,
            FAN_CREATE | FAN_DELETE | FAN_ONDIR,
            libc::AT_FDCWD,
            sv.as_ptr(),
        )
    } < 0
    {
        return Err(SupervisorError::FanotifySetup(io::Error::last_os_error()));
    }

    Ok(owned)
}

/// Read all pending fanotify events, parsing names from DFID_NAME info records.
pub fn read_fanotify_events(fd: &OwnedFd) -> Result<Vec<FanotifyEvent>, SupervisorError> {
    let mut events = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAGAIN) {
                break;
            }
            return Err(SupervisorError::FanotifyRead(err));
        }
        if n == 0 {
            break;
        }

        let mut offset = 0usize;
        let total = n as usize;
        while offset + 24 <= total {
            // Parse fanotify_event_metadata (24 bytes)
            let event_len =
                u32::from_ne_bytes(buf[offset..offset + 4].try_into().unwrap()) as usize;
            if event_len < 24 || offset + event_len > total {
                break;
            }
            let mask = u64::from_ne_bytes(buf[offset + 8..offset + 16].try_into().unwrap());
            let metadata_len = 24usize;

            // Parse info records after metadata
            if let Some(name) = parse_dfid_name(&buf[offset + metadata_len..offset + event_len]) {
                events.push(FanotifyEvent { mask, name });
            }

            offset += event_len;
        }
    }

    Ok(events)
}

/// Extract the filename from a DFID_NAME info record.
fn parse_dfid_name(data: &[u8]) -> Option<String> {
    if data.len() < 4 {
        return None;
    }
    let info_type = data[0];
    if info_type != FAN_EVENT_INFO_TYPE_DFID_NAME {
        return None;
    }
    let info_len = u16::from_ne_bytes([data[2], data[3]]) as usize;
    if data.len() < info_len || info_len < 20 {
        return None;
    }

    // Layout after header(4): fsid(8) + handle_bytes(4) + handle_type(4) + f_handle[N] + name
    let handle_bytes = u32::from_ne_bytes(data[12..16].try_into().unwrap()) as usize;
    let name_start = 20 + handle_bytes; // 4(hdr) + 8(fsid) + 4(handle_bytes) + 4(handle_type) + N
    if name_start >= info_len {
        return None;
    }

    let name_data = &data[name_start..info_len];
    let name_end = name_data
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_data.len());
    if name_end == 0 {
        return None;
    }

    String::from_utf8(name_data[..name_end].to_vec()).ok()
}

// ═══════════════════════════════════════════════════════════════════════════════
// Landlock — filesystem + network sandboxing
// ═══════════════════════════════════════════════════════════════════════════════

// Filesystem access rights
pub const LL_FS_EXECUTE: u64 = 1 << 0;
pub const LL_FS_WRITE_FILE: u64 = 1 << 1;
pub const LL_FS_READ_FILE: u64 = 1 << 2;
pub const LL_FS_READ_DIR: u64 = 1 << 3;
pub const LL_FS_REMOVE_DIR: u64 = 1 << 4;
pub const LL_FS_REMOVE_FILE: u64 = 1 << 5;
#[allow(dead_code)]
pub const LL_FS_MAKE_CHAR: u64 = 1 << 6;
pub const LL_FS_MAKE_DIR: u64 = 1 << 7;
pub const LL_FS_MAKE_REG: u64 = 1 << 8;
#[allow(dead_code)]
pub const LL_FS_MAKE_SOCK: u64 = 1 << 9;
#[allow(dead_code)]
pub const LL_FS_MAKE_FIFO: u64 = 1 << 10;
#[allow(dead_code)]
pub const LL_FS_MAKE_BLOCK: u64 = 1 << 11;
pub const LL_FS_MAKE_SYM: u64 = 1 << 12;
#[allow(dead_code)]
pub const LL_FS_REFER: u64 = 1 << 13;
pub const LL_FS_TRUNCATE: u64 = 1 << 14;
#[allow(dead_code)]
pub const LL_FS_IOCTL_DEV: u64 = 1 << 15;
pub const LL_FS_ALL: u64 = (1 << 16) - 1;

// Network access rights
pub const LL_NET_BIND_TCP: u64 = 1 << 0;
pub const LL_NET_CONNECT_TCP: u64 = 1 << 1;
pub const LL_NET_ALL: u64 = (1 << 2) - 1;

// Scoping
pub const LL_SCOPE_ABSTRACT_UNIX: u64 = 1 << 0;
pub const LL_SCOPE_SIGNAL: u64 = 1 << 1;

const LANDLOCK_RULE_PATH_BENEATH: libc::c_int = 1;
const LANDLOCK_RULE_NET_PORT: libc::c_int = 2;
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;

/// Query the Landlock ABI version supported by the running kernel.
/// Returns 1-6 for supported ABIs, or 0 if Landlock is unavailable.
pub fn landlock_abi_version() -> u32 {
    let ret = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if ret < 0 {
        0
    } else {
        ret as u32
    }
}

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

#[repr(C)]
struct LandlockNetPortAttr {
    allowed_access: u64,
    port: u64,
}

/// Create a Landlock ruleset. Returns the ruleset fd.
pub fn landlock_create_ruleset(
    fs_access: u64,
    net_access: u64,
    scoped: u64,
) -> io::Result<OwnedFd> {
    let attr = LandlockRulesetAttr {
        handled_access_fs: fs_access,
        handled_access_net: net_access,
        scoped,
    };
    let fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const LandlockRulesetAttr,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        ) as i32
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Add a path-beneath rule to a Landlock ruleset.
///
/// Landlock requires the fd to be a directory. If the path is a regular file,
/// we automatically use its parent directory instead (slightly more permissive
/// but correct — Landlock is directory-scoped by design).
pub fn landlock_add_rule_path(ruleset_fd: &OwnedFd, path: &Path, access: u64) -> io::Result<()> {
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains null byte"))?;
    let mut fd = unsafe { libc::open(path_c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    // Landlock RULE_PATH_BENEATH requires a directory fd.
    // If the path is a regular file, use its parent directory.
    let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat_buf) } == 0
        && (stat_buf.st_mode & libc::S_IFMT) == libc::S_IFREG
    {
        unsafe {
            libc::close(fd);
        }
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => return Ok(()), // no parent — skip rule
        };
        let parent_c = CString::new(parent.as_os_str().as_encoded_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains null byte"))?;
        fd = unsafe { libc::open(parent_c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
    }

    let attr = LandlockPathBeneathAttr {
        allowed_access: access,
        parent_fd: fd,
    };
    let ret = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset_fd.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &attr as *const LandlockPathBeneathAttr,
            0u32,
        )
    };
    unsafe {
        libc::close(fd);
    }
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Add a network port rule to a Landlock ruleset.
pub fn landlock_add_rule_net(ruleset_fd: &OwnedFd, port: u16, access: u64) -> io::Result<()> {
    let attr = LandlockNetPortAttr {
        allowed_access: access,
        port: port as u64,
    };
    let ret = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset_fd.as_raw_fd(),
            LANDLOCK_RULE_NET_PORT,
            &attr as *const LandlockNetPortAttr,
            0u32,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Enforce the Landlock ruleset on the current process.
/// Must call prctl(PR_SET_NO_NEW_PRIVS) first.
pub fn landlock_restrict_self(ruleset_fd: &OwnedFd) -> io::Result<()> {
    // PR_SET_NO_NEW_PRIVS is required before landlock_restrict_self
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let ret = unsafe { libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd.as_raw_fd(), 0u32) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Seccomp — syscall filtering
// ═══════════════════════════════════════════════════════════════════════════════

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}
#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

const SECCOMP_SET_MODE_FILTER: libc::c_uint = 1;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO_EPERM: u32 = 0x0005_0000 | 1; // SECCOMP_RET_ERRNO | EPERM

// BPF opcodes
const BPF_LD_W_ABS: u16 = 0x20; // BPF_LD | BPF_W | BPF_ABS
const BPF_JMP_JEQ_K: u16 = 0x15; // BPF_JMP | BPF_JEQ | BPF_K
const BPF_RET_K: u16 = 0x06; // BPF_RET | BPF_K

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

// libc's musl/aarch64 bindings omit SYS_kexec_file_load (aarch64 syscall 294)
#[cfg(all(target_arch = "aarch64", target_env = "musl"))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = 294;
#[cfg(not(all(target_arch = "aarch64", target_env = "musl")))]
const SYS_KEXEC_FILE_LOAD: libc::c_long = libc::SYS_kexec_file_load;

/// Denied syscalls for the default seccomp profile.
/// These are dangerous for containerized services and rarely needed.
#[cfg(target_arch = "x86_64")]
const DENIED_SYSCALLS: &[libc::c_long] = &[
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    SYS_KEXEC_FILE_LOAD,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_pivot_root,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_open_by_handle_at,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kcmp,
    libc::SYS_add_key,
    libc::SYS_keyctl,
    libc::SYS_request_key,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_acct,
    libc::SYS_lookup_dcookie,
];

/// aarch64 deny list — same minus lookup_dcookie (not an aarch64 syscall).
#[cfg(target_arch = "aarch64")]
const DENIED_SYSCALLS: &[libc::c_long] = &[
    libc::SYS_reboot,
    libc::SYS_kexec_load,
    SYS_KEXEC_FILE_LOAD,
    libc::SYS_init_module,
    libc::SYS_finit_module,
    libc::SYS_delete_module,
    libc::SYS_pivot_root,
    libc::SYS_swapon,
    libc::SYS_swapoff,
    libc::SYS_mount,
    libc::SYS_umount2,
    libc::SYS_open_by_handle_at,
    libc::SYS_ptrace,
    libc::SYS_process_vm_readv,
    libc::SYS_process_vm_writev,
    libc::SYS_kcmp,
    libc::SYS_add_key,
    libc::SYS_keyctl,
    libc::SYS_request_key,
    libc::SYS_bpf,
    libc::SYS_perf_event_open,
    libc::SYS_acct,
];

/// Install the default seccomp deny-list filter.
/// Blocks dangerous syscalls, allows everything else.
pub fn apply_seccomp_default() -> io::Result<()> {
    // PR_SET_NO_NEW_PRIVS is required (may already be set by landlock)
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };

    let mut filter: Vec<SockFilter> = Vec::new();
    let deny_count = DENIED_SYSCALLS.len();

    // Load architecture
    filter.push(SockFilter {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: 4,
    }); // offsetof(seccomp_data, arch)
        // If wrong arch → kill (jump over all instructions to deny)
    let total_after_arch = 1 + deny_count + 1; // load_nr + N deny checks + allow
    filter.push(SockFilter {
        code: BPF_JMP_JEQ_K,
        jt: 0,
        jf: total_after_arch as u8,
        k: AUDIT_ARCH,
    });

    // Load syscall number
    filter.push(SockFilter {
        code: BPF_LD_W_ABS,
        jt: 0,
        jf: 0,
        k: 0,
    }); // offsetof(seccomp_data, nr)

    // For each denied syscall: if match, jump to deny
    for (i, &nr) in DENIED_SYSCALLS.iter().enumerate() {
        let remaining = deny_count - i - 1;
        filter.push(SockFilter {
            code: BPF_JMP_JEQ_K,
            jt: (remaining + 1) as u8, // jump to DENY (past remaining checks + ALLOW)
            jf: 0,                     // continue checking
            k: nr as u32,
        });
    }

    // Default: allow
    filter.push(SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ALLOW,
    });
    // Deny
    filter.push(SockFilter {
        code: BPF_RET_K,
        jt: 0,
        jf: 0,
        k: SECCOMP_RET_ERRNO_EPERM,
    });

    let prog = SockFprog {
        len: filter.len() as u16,
        filter: filter.as_ptr(),
    };
    let ret = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            SECCOMP_SET_MODE_FILTER,
            0u32,
            &prog as *const SockFprog,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Capabilities — drop all except listed
// ═══════════════════════════════════════════════════════════════════════════════

const CAP_LAST_CAP: u32 = 41;

/// Map capability name (from sandbox config) to number.
pub fn cap_name_to_number(name: &str) -> Option<u32> {
    let name = name.strip_prefix("cap_").unwrap_or(name);
    match name {
        "chown" => Some(0),
        "dac_override" => Some(1),
        "dac_read_search" => Some(2),
        "fowner" => Some(3),
        "fsetid" => Some(4),
        "kill" => Some(5),
        "setgid" => Some(6),
        "setuid" => Some(7),
        "setpcap" => Some(8),
        "linux_immutable" => Some(9),
        "net_bind_service" => Some(10),
        "net_broadcast" => Some(11),
        "net_admin" => Some(12),
        "net_raw" => Some(13),
        "ipc_lock" => Some(14),
        "ipc_owner" => Some(15),
        "sys_module" => Some(16),
        "sys_rawio" => Some(17),
        "sys_chroot" => Some(18),
        "sys_ptrace" => Some(19),
        "sys_pacct" => Some(20),
        "sys_admin" => Some(21),
        "sys_boot" => Some(22),
        "sys_nice" => Some(23),
        "sys_resource" => Some(24),
        "sys_time" => Some(25),
        "sys_tty_config" => Some(26),
        "mknod" => Some(27),
        "lease" => Some(28),
        "audit_write" => Some(29),
        "audit_control" => Some(30),
        "setfcap" => Some(31),
        "mac_override" => Some(32),
        "mac_admin" => Some(33),
        "syslog" => Some(34),
        "wake_alarm" => Some(35),
        "block_suspend" => Some(36),
        "audit_read" => Some(37),
        "perfmon" => Some(38),
        "bpf" => Some(39),
        "checkpoint_restore" => Some(40),
        _ => None,
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Process signals
// ═══════════════════════════════════════════════════════════════════════════════

/// Send a signal to a process by PID.
pub fn kill_service(pid: u32, sig: i32) -> io::Result<()> {
    let ret = unsafe { libc::kill(pid as libc::pid_t, sig) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Pipe I/O — read/write for log routing
// ═══════════════════════════════════════════════════════════════════════════════

/// Read from a pipe. Returns 0 on EAGAIN (no data) or EOF.
pub fn read_pipe(fd: &OwnedFd, buf: &mut [u8]) -> io::Result<usize> {
    let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EAGAIN) {
            return Ok(0);
        }
        return Err(err);
    }
    Ok(n as usize)
}

/// Write all data to an fd, retrying on EINTR.
pub fn write_all(fd: &OwnedFd, data: &[u8]) -> io::Result<()> {
    let mut written = 0;
    while written < data.len() {
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                data[written..].as_ptr().cast(),
                data.len() - written,
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        written += n as usize;
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Process wait — waitid with pidfd
// ═══════════════════════════════════════════════════════════════════════════════

const P_PIDFD: libc::idtype_t = 3;

/// Wait for a child process via its pidfd. Returns (exit_code, signal).
/// Uses WNOHANG so it never blocks.
pub fn waitid_pidfd(
    pidfd: &OwnedFd,
) -> Result<(Option<i32>, Option<i32>), crate::error::SupervisorError> {
    let mut siginfo: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::waitid(
            P_PIDFD,
            pidfd.as_raw_fd() as libc::id_t,
            &mut siginfo,
            libc::WEXITED | libc::WNOHANG,
        )
    };
    if ret < 0 {
        return Err(crate::error::SupervisorError::WaitId(
            io::Error::last_os_error(),
        ));
    }
    // Extract si_code and si_status from siginfo_t.
    // On Linux, si_code is at offset 8, si_status at offset 24.
    let si_code = siginfo.si_code;
    let si_status =
        unsafe { *(((&siginfo) as *const libc::siginfo_t as *const u8).add(24) as *const i32) };
    match si_code {
        libc::CLD_EXITED => Ok((Some(si_status), None)),
        libc::CLD_KILLED | libc::CLD_DUMPED => Ok((None, Some(si_status))),
        _ => Ok((None, None)),
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Timeout allocation — for io_uring restart timers
// ═══════════════════════════════════════════════════════════════════════════════

/// Allocate a Timespec on the heap for io_uring timeout SQEs.
/// Returns (raw pointer for the SQE, opaque handle for free_timespec).
/// The pointer remains valid until free_timespec is called.
pub fn alloc_timespec(secs: u64) -> (*const io_uring::types::Timespec, u64) {
    let ts = Box::new(io_uring::types::Timespec::new().sec(secs));
    let ptr = Box::into_raw(ts);
    (ptr as *const _, ptr as u64)
}

/// Free a Timespec previously allocated by alloc_timespec.
pub fn free_timespec(handle: u64) {
    if handle != 0 {
        unsafe {
            drop(Box::from_raw(handle as *mut io_uring::types::Timespec));
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// Timerfd — periodic timer for deps polling
// ═══════════════════════════════════════════════════════════════════════════════

/// Create a repeating timerfd that fires every `interval_secs` seconds.
pub fn create_timerfd(interval_secs: u64) -> Result<OwnedFd, SupervisorError> {
    let fd = unsafe {
        libc::timerfd_create(
            libc::CLOCK_MONOTONIC,
            libc::TFD_NONBLOCK | libc::TFD_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(SupervisorError::RingInit(io::Error::last_os_error()));
    }
    let spec = libc::itimerspec {
        it_interval: libc::timespec {
            tv_sec: interval_secs as i64,
            tv_nsec: 0,
        },
        it_value: libc::timespec {
            tv_sec: interval_secs as i64,
            tv_nsec: 0,
        },
    };
    let ret = unsafe { libc::timerfd_settime(fd, 0, &spec, std::ptr::null_mut()) };
    if ret < 0 {
        unsafe { libc::close(fd) };
        return Err(SupervisorError::RingInit(io::Error::last_os_error()));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Read the timerfd to consume the timer expiration count.
pub fn read_timerfd(fd: &OwnedFd) {
    let mut buf = [0u8; 8];
    unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, 8) };
}

// ═══════════════════════════════════════════════════════════════════════════════
// Capabilities — drop all except listed
// ═══════════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════════
// Socket binding — for socket activation
// ═══════════════════════════════════════════════════════════════════════════════

/// Bind a TCP socket on 0.0.0.0:port. Returns the listening fd.
pub fn bind_tcp(port: u16) -> io::Result<OwnedFd> {
    let listener = std::net::TcpListener::bind(("0.0.0.0", port))?;
    listener.set_nonblocking(true)?;
    Ok(OwnedFd::from(listener))
}

/// Bind a TCP/TCP6 socket on the given address. Returns the listening fd.
pub fn bind_tcp_addr(addr: std::net::SocketAddr) -> io::Result<OwnedFd> {
    let listener = std::net::TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(OwnedFd::from(listener))
}

/// Bind a Unix domain socket. Unlinks any existing socket file first.
pub fn bind_unix(path: &Path) -> io::Result<OwnedFd> {
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(OwnedFd::from(listener))
}

/// Accept a connection on a listening socket (SOCK_CLOEXEC).
#[allow(dead_code)]
pub fn accept_fd(listen_fd: &OwnedFd) -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::accept4(
            listen_fd.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

// ═══════════════════════════════════════════════════════════════════════════════
// Mount namespace — private propagation, read-only root, private tmp, new mount API
// ═══════════════════════════════════════════════════════════════════════════════

/// Set mount propagation to private on /, preventing mount events leaking to host.
/// Tolerates EINVAL (already private or no mount namespace).
pub fn make_private_propagation() -> io::Result<()> {
    let ret = unsafe {
        libc::mount(
            std::ptr::null(),
            b"/\0".as_ptr().cast(),
            std::ptr::null(),
            libc::MS_REC | libc::MS_PRIVATE,
            std::ptr::null(),
        )
    };
    if ret < 0 {
        let err = io::Error::last_os_error();
        // EINVAL means already private or no mount namespace — not an error
        if err.raw_os_error() == Some(libc::EINVAL) {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// Remount / as read-only in the current mount namespace.
///
/// Sequence: bind-mount / onto itself (non-recursive to avoid mount limit),
/// then remount that as read-only. Tolerates ENOSPC (mount table full).
pub fn remount_readonly_root() -> io::Result<()> {
    // Step 1: non-recursive bind-mount / onto itself (avoids cloning all submounts)
    let ret = unsafe {
        libc::mount(
            b"/\0".as_ptr().cast(),
            b"/\0".as_ptr().cast(),
            std::ptr::null(),
            libc::MS_BIND,
            std::ptr::null(),
        )
    };
    if ret < 0 {
        let err = io::Error::last_os_error();
        // ENOSPC = mount table full, EPERM = no privilege — non-fatal
        if err.raw_os_error() == Some(libc::ENOSPC) || err.raw_os_error() == Some(libc::EPERM) {
            return Ok(());
        }
        return Err(err);
    }

    // Step 2: remount the bind as read-only
    let ret = unsafe {
        libc::mount(
            b"/\0".as_ptr().cast(),
            b"/\0".as_ptr().cast(),
            std::ptr::null(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
            std::ptr::null(),
        )
    };
    if ret < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::ENOSPC) {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

/// Mount a tmpfs at the given path (e.g., /tmp). Size limited to 64MB.
pub fn mount_tmpfs(path: &Path) -> io::Result<()> {
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "null in path"))?;
    let ret = unsafe {
        libc::mount(
            b"tmpfs\0".as_ptr().cast(),
            path_c.as_ptr(),
            b"tmpfs\0".as_ptr().cast(),
            libc::MS_NOSUID | libc::MS_NODEV,
            b"size=64M,mode=1777\0".as_ptr().cast(),
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// --- New mount API syscall wrappers (kernel 5.2+) ---
// These are plumbed for ID-mapped mounts but not yet wired into the
// mount namespace setup. Wiring requires creating user namespaces with
// specific UID/GID mappings — follow-up work.

#[allow(dead_code)]
const SYS_OPEN_TREE: libc::c_long = 428;
#[allow(dead_code)]
const SYS_MOVE_MOUNT: libc::c_long = 429;
#[allow(dead_code)]
const SYS_MOUNT_SETATTR: libc::c_long = 442;

#[allow(dead_code)]
const OPEN_TREE_CLONE: libc::c_uint = 1;
#[allow(dead_code)]
const AT_RECURSIVE: libc::c_uint = 0x8000;
#[allow(dead_code)]
const MOUNT_ATTR_IDMAP: u64 = 0x0010_0000;
#[allow(dead_code)]
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x0000_0004;

#[allow(dead_code)]
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// Clone a mount subtree into an fd (new mount API).
#[allow(dead_code)]
pub fn open_tree(path: &Path) -> io::Result<OwnedFd> {
    let path_c = CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "null in path"))?;
    let fd = unsafe {
        libc::syscall(
            SYS_OPEN_TREE,
            libc::AT_FDCWD,
            path_c.as_ptr(),
            OPEN_TREE_CLONE | AT_RECURSIVE | libc::O_CLOEXEC as libc::c_uint,
        ) as i32
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Apply an ID mapping to a cloned mount via a user namespace fd.
#[allow(dead_code)]
pub fn mount_setattr_idmap(mount_fd: &OwnedFd, userns_fd: &OwnedFd) -> io::Result<()> {
    let attr = MountAttr {
        attr_set: MOUNT_ATTR_IDMAP,
        attr_clr: 0,
        propagation: 0,
        userns_fd: userns_fd.as_raw_fd() as u64,
    };
    let ret = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            mount_fd.as_raw_fd(),
            b"\0".as_ptr(), // AT_EMPTY_PATH
            libc::AT_EMPTY_PATH,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Attach a cloned mount at a path (new mount API).
#[allow(dead_code)]
pub fn move_mount(from_fd: &OwnedFd, to_path: &Path) -> io::Result<()> {
    let to_c = CString::new(to_path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "null in path"))?;
    let ret = unsafe {
        libc::syscall(
            SYS_MOVE_MOUNT,
            from_fd.as_raw_fd(),
            b"\0".as_ptr(), // empty source path (use the fd)
            libc::AT_FDCWD,
            to_c.as_ptr(),
            MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Cgroup file writes + PSI triggers
// ═══════════════════════════════════════════════════════════════════════════════

/// Write a value to a cgroup interface file.
pub fn write_cgroup_file(cgroup_path: &str, filename: &str, value: &str) -> io::Result<()> {
    let path = format!("{cgroup_path}/{filename}");
    std::fs::write(&path, value).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))
}

/// Open a cgroup PSI pressure file with a trigger, returning a pollable fd.
/// Trigger format: "some 100000 1000000" (stall_type threshold_us window_us).
/// The fd becomes POLLPRI-readable when pressure exceeds the threshold.
pub fn setup_psi_trigger(cgroup_path: &str, resource: &str, trigger: &str) -> io::Result<OwnedFd> {
    let path_str = format!("{cgroup_path}/{resource}.pressure");
    let path_c = CString::new(path_str.clone())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "null in path"))?;

    let fd = unsafe {
        libc::open(
            path_c.as_ptr(),
            libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    // Write trigger definition — this arms the PSI notification
    let n = unsafe { libc::write(owned.as_raw_fd(), trigger.as_ptr().cast(), trigger.len()) };
    if n < 0 {
        return Err(io::Error::new(
            io::Error::last_os_error().kind(),
            format!("writing PSI trigger to {path_str}"),
        ));
    }

    Ok(owned)
}

// ═══════════════════════════════════════════════════════════════════════════════
// Capabilities — drop all except listed
// ═══════════════════════════════════════════════════════════════════════════════

/// Drop all capabilities except those listed. Called in child before exec.
pub fn drop_capabilities(keep: &[String]) -> io::Result<()> {
    let keep_set: Vec<u32> = keep.iter().filter_map(|n| cap_name_to_number(n)).collect();

    // Drop from bounding set
    for cap in 0..=CAP_LAST_CAP {
        if !keep_set.contains(&cap) {
            // Ignore errors — cap may already be dropped or not in bounding set
            unsafe { libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) };
        }
    }

    // Build capability bitmask for kept caps
    let mut effective: [u32; 2] = [0, 0];
    let mut permitted: [u32; 2] = [0, 0];
    for &cap in &keep_set {
        let idx = (cap / 32) as usize;
        let bit = 1u32 << (cap % 32);
        if idx < 2 {
            effective[idx] |= bit;
            permitted[idx] |= bit;
        }
    }

    // capset syscall — _LINUX_CAPABILITY_VERSION_3 = 0x20080522
    #[repr(C)]
    struct CapHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    struct CapData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let header = CapHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [
        CapData {
            effective: effective[0],
            permitted: permitted[0],
            inheritable: 0,
        },
        CapData {
            effective: effective[1],
            permitted: permitted[1],
            inheritable: 0,
        },
    ];

    let ret =
        unsafe { libc::syscall(libc::SYS_capset, &header as *const CapHeader, data.as_ptr()) };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
