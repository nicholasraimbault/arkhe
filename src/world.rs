//! World — the ECS data store
//!
//! All service state lives here as parallel arrays indexed by ServiceId.
//! Systems receive `&mut World` and operate on specific component combinations.
//! No global state. No singletons. One World per supervisor instance.

use std::collections::{HashMap, HashSet};
use std::os::fd::{OwnedFd, RawFd};

use crate::components::*;

/// Entity identifier. Index into parallel arrays in World.
pub type ServiceId = usize;

/// All service state. Parallel arrays indexed by ServiceId.
pub struct World {
    // === Global state ===
    /// Landlock ABI version detected at startup. 0 = unavailable.
    pub landlock_abi: u32,

    // === Identity ===
    pub names: Vec<String>,

    // === Static config (loaded from /etc/sv/ at boot, immutable at runtime) ===
    pub run_configs: Vec<RunConfig>,
    pub sandbox_configs: Vec<SandboxConfig>,
    pub dependencies: Vec<Option<Dependencies>>,
    pub resource_limits: Vec<Option<ResourceLimits>>,
    pub listen_sockets: Vec<Option<ListenSockets>>,
    pub log_configs: Vec<LogConfig>,

    // === Runtime state ===
    pub states: Vec<RuntimeState>,
    pub readiness: Vec<ReadinessState>,
    pub pidfds: Vec<Option<OwnedFd>>,
    pub cgroup_handles: Vec<Option<CgroupHandle>>,
    pub log_pipe_fds: Vec<Option<OwnedFd>>,
    pub log_file_fds: Vec<Option<OwnedFd>>,
    pub restart_states: Vec<RestartState>,
    pub restart_timeout_ptrs: Vec<u64>,
    /// Pre-bound listening socket fds for socket-activated services.
    pub listen_fds: Vec<Vec<OwnedFd>>,

    // === Reverse lookup maps (fd → ServiceId) ===
    pub socket_map: HashMap<RawFd, ServiceId>,
    pub psi_map: HashMap<RawFd, ServiceId>,

    // === Polling fallback state (used when fanotify is unavailable) ===
    pub known_ready_files: HashSet<String>,
    pub known_service_dirs: HashSet<String>,
    /// Heap-allocated Timespec for the polling timeout SQE.
    pub deps_poll_timeout_ptr: u64,
}

impl World {
    pub fn new() -> Self {
        World {
            landlock_abi: 0,
            names: Vec::new(),
            run_configs: Vec::new(),
            sandbox_configs: Vec::new(),
            dependencies: Vec::new(),
            resource_limits: Vec::new(),
            listen_sockets: Vec::new(),
            log_configs: Vec::new(),
            states: Vec::new(),
            readiness: Vec::new(),
            pidfds: Vec::new(),
            cgroup_handles: Vec::new(),
            log_pipe_fds: Vec::new(),
            log_file_fds: Vec::new(),
            restart_states: Vec::new(),
            restart_timeout_ptrs: Vec::new(),
            listen_fds: Vec::new(),
            socket_map: HashMap::new(),
            psi_map: HashMap::new(),
            known_ready_files: HashSet::new(),
            known_service_dirs: HashSet::new(),
            deps_poll_timeout_ptr: 0,
        }
    }

    /// Add a new service to the world. Returns its ServiceId.
    pub fn add_service(&mut self, name: String, run_config: RunConfig) -> ServiceId {
        let id = self.names.len();
        self.names.push(name);
        self.run_configs.push(run_config);
        self.sandbox_configs.push(SandboxConfig::strict_default());
        self.dependencies.push(None);
        self.resource_limits.push(None);
        self.listen_sockets.push(None);
        self.log_configs.push(LogConfig::default());
        self.states.push(RuntimeState::NotStarted);
        self.readiness.push(ReadinessState::default());
        self.pidfds.push(None);
        self.cgroup_handles.push(None);
        self.log_pipe_fds.push(None);
        self.log_file_fds.push(None);
        self.restart_states.push(RestartState::default());
        self.restart_timeout_ptrs.push(0);
        self.listen_fds.push(Vec::new());
        id
    }

    /// Number of services in the world.
    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Find service by name. Linear scan — fine for <200 services.
    pub fn find_by_name(&self, name: &str) -> Option<ServiceId> {
        self.names.iter().position(|n| n == name)
    }
}
