//! Component types — per-service data
//!
//! Each component corresponds to a file in /etc/sv/<name>/.
//! Components are pure data. No methods that mutate global state.
//! Systems operate on components via &mut World.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// === Static config components (loaded at boot) ===

/// Parsed from /etc/sv/<name>/run (and optionally /finish)
pub struct RunConfig {
    pub run_path: PathBuf,
    pub finish_path: Option<PathBuf>,
    pub enabled: bool,
}

/// Parsed from /etc/sv/<name>/sandbox
/// Defaults to strict (deny all). Users weaken explicitly.
pub struct SandboxConfig {
    pub read_paths: Vec<PathBuf>,
    pub write_paths: Vec<PathBuf>,
    pub exec_paths: Vec<PathBuf>,
    pub bind_ports: Vec<u16>,
    pub connect_ports: Vec<u16>,
    pub pid_namespace: bool,
    pub mount_namespace: bool,
    pub ipc_namespace: bool,
    pub uts_namespace: bool,
    pub network_namespace: NetworkNamespace,
    pub private_tmp: bool,
    pub read_only_root: bool,
    pub capabilities: Vec<String>,
    pub seccomp_profile: SeccompProfile,
    pub ipc_scope: IpcScope,
    pub permissive: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum NetworkNamespace {
    Private,
    Host,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeccompProfile {
    Default,  // @system-service whitelist
    Disabled, // no seccomp (only in permissive mode)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum IpcScope {
    Scoped, // landlock: restrict abstract unix sockets + signals
    Unscoped,
}

impl SandboxConfig {
    /// Default strict sandbox — deny everything.
    pub fn strict_default() -> Self {
        SandboxConfig {
            read_paths: Vec::new(),
            write_paths: Vec::new(),
            exec_paths: Vec::new(),
            bind_ports: Vec::new(),
            connect_ports: Vec::new(),
            pid_namespace: true,
            mount_namespace: true,
            ipc_namespace: true,
            uts_namespace: true,
            network_namespace: NetworkNamespace::Private,
            private_tmp: true,
            read_only_root: true,
            capabilities: Vec::new(),
            seccomp_profile: SeccompProfile::Default,
            ipc_scope: IpcScope::Scoped,
            permissive: false,
        }
    }
}

/// Parsed from /etc/sv/<name>/depends
pub struct Dependencies {
    pub names: Vec<String>,
}

/// Parsed from /etc/sv/<name>/resources
pub struct ResourceLimits {
    pub memory_max: Option<u64>,     // bytes
    pub memory_high: Option<u64>,    // bytes
    pub cpu_weight: Option<u32>,     // 1-10000
    pub cpu_max: Option<(u64, u64)>, // (quota_us, period_us)
    pub io_weight: Option<u32>,      // 1-10000
    pub pids_max: Option<u32>,
}

/// Parsed from /etc/sv/<name>/listen
pub struct ListenSockets {
    pub sockets: Vec<ListenAddr>,
}

pub enum ListenAddr {
    Tcp(u16),
    Tcp6(std::net::SocketAddr),
    Unix(PathBuf),
}

/// Parsed from /etc/sv/<name>/log/config (or defaults)
pub struct LogConfig {
    pub max_size: u64, // bytes
    pub max_files: u32,
}

impl Default for LogConfig {
    fn default() -> Self {
        LogConfig {
            max_size: 1_048_576, // 1 MB
            max_files: 10,
        }
    }
}

// === Runtime state components (change during operation) ===

/// Current state of a service process.
/// Pidfd ownership lives in World.pidfds, not here (OwnedFd isn't Copy).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RuntimeState {
    NotStarted,
    Starting,
    Running {
        pid: u32,
        started_at: Instant,
    },
    Ready {
        pid: u32,
        started_at: Instant,
    },
    Stopping {
        pid: u32,
        stop_requested_at: Instant,
    },
    Stopped {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
    Failing {
        last_exit: Option<i32>,
        restart_count: u32,
        next_restart: Instant,
    },
}

/// Readiness tracking for a service
pub struct ReadinessState {
    pub mode: ReadinessMode,
    pub timeout: Duration,
    pub satisfied: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ReadinessMode {
    File,    // service creates /run/ready/<name>
    Fd,      // supervisor passes notification fd
    Timeout, // assume ready after timeout
}

impl Default for ReadinessState {
    fn default() -> Self {
        ReadinessState {
            mode: ReadinessMode::File,
            timeout: Duration::from_secs(30),
            satisfied: false,
        }
    }
}

/// Restart tracking for a service.
pub struct RestartState {
    pub count: u32,
    pub last_restart: Instant,
    pub backoff_secs: u64,
}

impl Default for RestartState {
    fn default() -> Self {
        RestartState {
            count: 0,
            last_restart: Instant::now(),
            backoff_secs: 0,
        }
    }
}

/// Runtime cgroup handle — OwnedFd for RAII cleanup.
pub struct CgroupHandle {
    pub cgroup_fd: OwnedFd,
    pub psi_memory_fd: Option<OwnedFd>,
    pub psi_cpu_fd: Option<OwnedFd>,
}
