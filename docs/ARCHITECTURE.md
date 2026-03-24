# arkhe — Architecture

## Overview

arkhe is decomposed into isolated components with minimal privilege. The core insight: if PID 1 has a bug, the kernel panics. Therefore PID 1 must be the simplest correct program possible, and everything else runs as supervised children.

## Data architecture: Entity-Component-System

arkhe uses a data-oriented ECS pattern — NOT an ECS framework. No bevy_ecs, no hecs, no specs. Just parallel arrays indexed by `ServiceId`. This is the structural foundation of the supervisor.

### Why ECS

The key insight: organize the program around data transformations, not object hierarchies. A Service in OOP would be a struct with methods — state and behavior coupled together. In ECS, state (components) and behavior (systems) are separate. This produces code that answers the questions humans actually ask: "what happens when a process exits?" → read `systems/supervise.rs`. "What can be sandboxed?" → read `sandbox.rs`.

Benefits for arkhe specifically:
- **Compositional services:** Not every service has every feature. A service without a `listen` file has no `ListenSockets` component. Socket activation never sees it. With a monolithic struct, this would be `Option<T>` fields checked everywhere.
- **Independent testability:** Each system is a function that takes `&mut World`. No global state, no mock supervisor. SpawnSystem can be tested by constructing a World with specific components and asserting the result.
- **Extension without modification:** Adding health checks = adding a `HealthCheck` component + a `HealthSystem`. No existing system changes. This is how the codebase stays under 10K lines as capabilities grow.
- **1:1 mapping to disk format:** Each component type corresponds to a file in `/etc/sv/<n>/`. The on-disk representation IS the component model.

### World struct

```rust
type ServiceId = usize;

struct World {
    // Identity
    names: Vec<String>,

    // Static config (loaded from /etc/sv/ at boot, immutable at runtime)
    run_configs: Vec<RunConfig>,
    sandbox_configs: Vec<Option<SandboxConfig>>,
    dependencies: Vec<Option<Dependencies>>,
    resource_limits: Vec<Option<ResourceLimits>>,
    listen_sockets: Vec<Option<ListenSockets>>,
    log_configs: Vec<Option<LogConfig>>,

    // Runtime state (mutable, changes during operation)
    states: Vec<RuntimeState>,
    readiness: Vec<ReadinessState>,
    cgroup_handles: Vec<Option<CgroupHandle>>,

    // Reverse lookup maps (fd → ServiceId)
    pidfd_map: HashMap<RawFd, ServiceId>,
    socket_map: HashMap<RawFd, ServiceId>,
    psi_map: HashMap<RawFd, ServiceId>,
}
```

Parallel arrays, indexed by ServiceId. `Option` wrapping for optional components. HashMap for reverse lookups from file descriptors to entities.

### System dispatch

The io_uring event loop IS the ECS scheduler. Each completion carries a user_data tag encoding the system and entity:

```rust
loop {
    ring.submit_and_wait(1);
    for cqe in ring.completion() {
        match decode_tag(cqe.user_data()) {
            Tag::Pidfd(id)   => systems::supervise::on_exit(&mut world, id, &mut ring),
            Tag::Inotify     => systems::deps::on_ready(&mut world, &cqe, &mut ring),
            Tag::Signal      => systems::signal::on_signal(&mut world, &cqe, &mut ring),
            Tag::Splice(id)  => systems::log::on_splice(&mut world, id, &mut ring),
            Tag::Accept(id)  => systems::socket::on_accept(&mut world, id, &mut ring),
            Tag::Psi(id)     => systems::pressure::on_pressure(&mut world, id, &mut ring),
        }
    }
}
```

Each system function receives `&mut World` and the io_uring ring (for submitting follow-up operations). Systems are plain functions, not trait objects. No dynamic dispatch. No allocation in the hot path.

## Component map

### PID 1 (pid1 binary)

**Purpose:** Satisfy kernel requirements for init. Nothing else.

**What it does:**
1. Sets up signal mask (block all signals except those we handle)
2. Execs the supervisor as its first and only child
3. Enters a loop: `waitpid(-1, &status, 0)` — reaps any zombie
4. On SIGTERM/SIGINT: forwards to supervisor, waits for exit, calls `reboot(RB_POWER_OFF)`

**What it does NOT do:**
- Parse config files
- Open network connections
- Allocate heap memory
- Process IPC messages

**Size target:** 200 lines of C or `#![no_std]` Rust.

**Compilation:** Static linking. No dynamic dependencies. `panic = "abort"`. No unwinding.

### Supervisor (arkhd binary)

**Purpose:** The actual init logic. Manages services, resolves dependencies, enforces sandboxes.

**Runs as:** root (necessary for clone3 with namespace flags and cgroup management)

**Internal structure:** ECS pattern. The World struct holds all service state as parallel arrays. Systems are plain functions dispatched from the io_uring event loop. See "Data architecture" section above.

**Event loop:** Single io_uring instance. All I/O is asynchronous through one ring. The ring IS the ECS scheduler — each completion is dispatched to the appropriate system.

The ring handles these event sources simultaneously:
- **pidfds** (IORING_OP_POLL_ADD, multishot) → SupervisionSystem
- **inotify fd** (IORING_OP_POLL_ADD, multishot) → DependencySystem
- **signalfd** (IORING_OP_POLL_ADD, multishot) → SignalSystem
- **log pipes** (IORING_OP_SPLICE) → LogSystem
- **socket activator fds** (IORING_OP_ACCEPT, multishot) → SocketSystem
- **PSI eventfds** (IORING_OP_POLL_ADD) → PressureSystem

One `io_uring_enter` call blocks until any event completes. The supervisor decodes the completion's user_data tag and calls the matching system function with `&mut World`.

### Service lifecycle

```
[not started] → [starting] → [running] → [ready] → [stopping] → [stopped]
                                  ↓                        ↓
                              [failing] ←──────────── [crashed]
                                  ↓
                           [backoff wait]
                                  ↓
                              [starting]
```

**Starting:** clone3 is called. Child sets up sandbox, execs service binary.

**Running:** Service process is alive. pidfd is being polled.

**Ready:** Service has signaled readiness (touched its readiness file, or readiness-fd protocol). Dependents can now start. `/run/ready/<service>` is created.

**Failing:** Service exited unexpectedly. Restart with exponential backoff (1s, 2s, 4s, 8s, max 60s).

**Stopping:** SIGTERM sent. Grace period (default 5s). Then SIGKILL.

### Dependency resolution

**Mechanism:** Filesystem presence via inotify.

A service declares dependencies in `/etc/sv/<name>/depends`:
```
network-online
dns-ready
```

The supervisor watches `/run/ready/` with inotify. When `/run/ready/network-online` appears (created by the networking service upon readiness), any service depending on `network-online` becomes eligible to start.

**No graph engine.** No topological sort. No cycle detection algorithm in the supervisor. The inotify watch is O(1) per event. The supervisor maintains a map of `dependency_name → Vec<service_name>` and on each inotify IN_CREATE event, checks if all dependencies for waiting services are now satisfied.

**Cycle detection:** Done once at boot by the CLI (`ark check`) as a pre-flight. Not in the supervisor's hot path.

### Readiness protocol

Services signal readiness by one of:
1. **File-based (default):** Service itself creates `/run/ready/<service>` when ready
2. **fd-based:** Supervisor passes a file descriptor; service writes to it when ready (similar to sd_notify but simpler)
3. **Timeout-based (fallback):** If no readiness signal within N seconds, assume ready

The readiness file is removed by the supervisor when the service exits.

### Log routing

Each service's stdout and stderr are connected to a pipe at spawn time. The supervisor uses `IORING_OP_SPLICE` to move data from the pipe directly to the log file in kernel space — zero copies through userspace.

Log files: `/var/log/sv/<service>/current`

Rotation: When `current` exceeds 1MB, rename to `current.1`, shift existing rotations, cap at 10 files. Configurable per-service.

The logger is NOT a separate process per service (unlike s6-log). It's handled by the supervisor's io_uring ring. This reduces process count and context switches while maintaining per-service log isolation.

### Socket activation

A standalone component within the supervisor (or optionally a separate supervised process).

Configuration: `/etc/sv/<service>/listen`
```
tcp:80
tcp:443
unix:/run/nginx.sock
```

On boot, the supervisor binds all declared sockets immediately (fast boot). When a connection arrives (detected via `IORING_OP_ACCEPT` multishot), the supervisor:
1. Starts the associated service if not running
2. Passes the listening fd to the service via `CLONE_PIDFD` + fd passing
3. The service accepts connections on the pre-bound socket

### cgroup v2 integration

Each service gets its own cgroup: `/sys/fs/cgroup/arkhe.slice/<service>.scope/`

Created before service spawn. Populated via `CLONE_INTO_CGROUP` flag on `clone3` — atomic, no race window.

Resource limits from `/etc/sv/<service>/resources`:
```
memory-max = 512M
cpu-weight = 100
io-weight = 100
pids-max = 64
```

Written to cgroup interface files before clone3.

**PSI monitoring:** Each service cgroup has PSI (Pressure Stall Information) triggers. The supervisor registers eventfd triggers for memory and CPU pressure thresholds. These eventfds are added to the io_uring ring. When pressure exceeds thresholds, the supervisor receives a completion event and can:
- Log a warning
- Surface it in `ark status` and `ark check`
- Optionally kill the service before OOM does (configurable)

### Status reporting

All runtime state is plain text files in `/run/arkhe/`:

```
/run/arkhe/
├── services/
│   ├── nginx          # "running 3h12m pid=1234 sandbox=strict ready=yes"
│   ├── postgres       # "running 3h11m pid=1235 sandbox=permissive ready=yes"
│   └── failedthing    # "failing exit=1 restarts=3 last='connection refused'"
├── ready/             # (symlink to /run/ready/ or same directory)
└── boot-time          # "0.847s"
```

The CLI reads these files. The supervisor writes them. No IPC protocol needed.

## Data flow

```
                                          /etc/sv/<service>/
                                               │
                                    ┌──────────┤
                                    │  run     │ depends │ sandbox │ resources │ listen
                                    │          │         │         │           │
                                    ▼          ▼         ▼         ▼           ▼
              ┌──────────────── SUPERVISOR (arkhd) ──────────────────┐
              │                                                      │
              │  io_uring ring:                                      │
              │  ┌─ pidfd polls (process exits)                      │
              │  ├─ inotify poll (/run/ready/ changes)               │
              │  ├─ signalfd poll (SIGCHLD, SIGTERM)                 │
              │  ├─ splice ops (log pipes → log files)               │
              │  ├─ accept ops (socket activation)                   │
              │  └─ PSI eventfds (resource pressure)                 │
              │                                                      │
              └───────┬──────────────────────────────────┬───────────┘
                      │                                  │
                      ▼                                  ▼
             Service processes                    /run/arkhe/
             (sandboxed, namespaced)              (plain text status)
                      │                                  │
                      ▼                                  ▼
             /var/log/sv/<service>/               `ark` CLI reads
             (plain text logs)                    these files
```

## Security boundaries

```
┌─────────────────────────────────────────────────┐
│ PID 1 (200 lines, root, no allocation)          │
│   └── exec → Supervisor                         │
├─────────────────────────────────────────────────┤
│ Supervisor (root, <10K lines, Rust safe code)   │
│   └── clone3 → Service                          │
├─────────────────────────────────────────────────┤
│ Service (sandboxed, unprivileged)               │
│   ├── Landlock: filesystem + network deny       │
│   ├── seccomp: syscall whitelist                │
│   ├── Namespaces: PID + mount + IPC             │
│   ├── Capabilities: dropped                     │
│   ├── cgroup: resource limits                   │
│   └── ID-mapped mount: uid/gid remapping        │
└─────────────────────────────────────────────────┘
```

The privilege boundary is between the supervisor and the service. The supervisor is the last trusted component. Everything below it is untrusted and contained.
