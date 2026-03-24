# CLAUDE.md — arkhe init system

## What is this project?

arkhe (ἀρχή — "origin", "first principle") is a Linux init system built on Pronoia Design principles. It is the first init system built entirely on post-2019 kernel APIs. It is a Rust project.

arkhe is NOT another systemd clone. It is NOT another minimal init. It is a new category: a pronoic init — structurally secure by default, effortless for users, and auditable by design.

## Design philosophy: Pronoia Design

Read `docs/PHILOSOPHY.md` for the full framework. The short version:

**Pronoia Design** is the discipline of building systems that are structurally on the user's side — where the architecture conspires for the user's benefit without requiring their awareness, configuration, or trust.

For arkhe specifically:
- Every service is sandboxed by default. There is no unsandboxed mode.
- The user drops a script in a directory and gets supervision + sandboxing + logging + dependency ordering for free.
- The entire codebase is under 10K lines. A single person can audit it in a day.
- Plain text everything. No binary formats. No custom protocols.
- The filesystem IS the IPC. No D-Bus. No message passing.

## Architecture overview

Read `docs/ARCHITECTURE.md` for full details. Summary:

```
PID 1 (200 lines, C or minimal Rust #![no_std])
  └── Supervisor (Rust, io_uring event loop)
        ├── Service spawner
        │     ├── clone3 (CLONE_PIDFD | CLONE_INTO_CGROUP | CLONE_NEWPID | CLONE_NEWNS)
        │     ├── close_range() — prevent fd leaks
        │     ├── New mount API (fsopen/fsmount/move_mount) — mount namespace
        │     ├── mount_setattr + MOUNT_ATTR_IDMAP — uid/gid mapping
        │     ├── Landlock ruleset — filesystem + network + IPC scoping
        │     ├── seccomp-bpf — syscall filter
        │     └── prctl — drop capabilities
        ├── Dependency resolver (inotify on /run/ready/)
        ├── Log router (io_uring splice, pipe → file)
        ├── Socket activator (io_uring multishot accept)
        ├── Resource monitor (PSI eventfd per service cgroup)
        └── Status reporter (plain text /run/arkhe/)
```

## Key technical decisions

### Language
- Supervisor and all tooling: Rust (stable, no nightly features)
- PID 1 stub: either C (200 lines) or Rust `#![no_std]` — whichever is simpler
- Zero `unsafe` blocks in the supervisor. All syscalls through `rustix` safe wrappers.
- `panic = "abort"` for PID 1. No unwinding. No allocation in PID 1.

### Kernel API surface (minimum kernel 6.12+)
- **pidfd**: `clone3` + `CLONE_PIDFD`, `waitid(P_PIDFD)`, pollable pidfds for process lifecycle
- **io_uring**: single ring for the entire supervisor event loop (process exits, inotify, signals, logs, sockets)
- **Landlock LSM**: default-deny filesystem + network + IPC scoping (ABI v6)
- **seccomp-bpf**: syscall whitelist per service
- **cgroup v2**: resource accounting, PSI monitoring, `CLONE_INTO_CGROUP` for atomic placement
- **inotify**: dependency resolution via `/run/ready/` file existence
- **signalfd**: signal handling in the event loop
- **Namespaces**: PID, mount, IPC, UTS, network — for process isolation
- **ID-mapped mounts**: `mount_setattr` + `MOUNT_ATTR_IDMAP` for per-service uid/gid mapping
- **New mount API**: `fsopen`/`fsmount`/`move_mount`/`open_tree` for mount namespace setup
- **close_range()**: prevent fd leaks into services
- **PSI**: Pressure Stall Information eventfd triggers per service cgroup

### Dependencies (target: <10 transitive crates)
- `rustix` — safe syscall wrappers (well-audited, Rust ecosystem team)
- `io-uring` — Rust io_uring bindings (tokio team)
- NO async runtime (no tokio, no async-std — hand-written io_uring loop)
- NO serde (no deserialization of untrusted formats)
- NO logging framework (services write to stdout, logger pipe handles it)

### What arkhe does NOT do
- No D-Bus
- No binary log format
- No DNS resolution
- No device management (separate process, like eudev)
- No login/seat management
- No timer scheduling in PID 1 (separate supervised service)
- No container management
- No home directory encryption

## Internal architecture: ECS pattern

arkhe uses an Entity-Component-System (ECS) pattern — NOT an ECS framework. No bevy_ecs, no hecs. Just parallel Vecs indexed by ServiceId. This is data-oriented design: organize around data transformations, not object hierarchies.

**Why ECS:**
- Not every service has every feature (sockets, dependencies, resource limits). Optional components handle this naturally.
- Each system (spawn, supervise, dependency resolution, logging) is an independent function operating on a subset of components. Independently testable.
- Adding features = adding a component + a system. No existing code changes. Keeps the codebase under 10K lines as capabilities grow.
- The architecture matches the on-disk format: each component corresponds to a file in `/etc/sv/<name>/`.

**Entity** = a service, identified by `ServiceId` (a `usize` index)

**Components** (per-service data, parallel arrays):
- `RunConfig` — run script path, finish script
- `Dependencies` — dependency name list
- `SandboxConfig` — Landlock rules, namespace flags, capabilities
- `ResourceLimits` — cgroup settings
- `ListenSockets` — socket activation config
- `LogConfig` — rotation settings
- `RuntimeState` — pid, pidfd, state enum, uptime, restart count
- `ReadinessState` — mode, timeout, satisfied
- `CgroupHandle` — cgroup fd, PSI eventfds

**Systems** (functions that iterate over entities with specific component combinations):
- `SpawnSystem` — starts services with satisfied dependencies
- `SupervisionSystem` — handles process exits, schedules restarts
- `DependencySystem` — updates dependency satisfaction on inotify events
- `LogSystem` — routes log data via io_uring splice
- `SocketSystem` — handles socket activation on accept events
- `PressureSystem` — monitors PSI eventfds, logs warnings
- `StatusSystem` — writes plain text status to /run/arkhe/

**World struct** holds all component storage + reverse lookup maps (pidfd→ServiceId, socket_fd→ServiceId, psi_fd→ServiceId).

**The io_uring event loop IS the ECS scheduler.** Each ring completion is dispatched to the appropriate system based on the user_data tag.

## File structure

```
arkhe/
├── CLAUDE.md                  # This file
├── Cargo.toml
├── docs/
│   ├── PHILOSOPHY.md          # Pronoia Design framework
│   ├── ARCHITECTURE.md        # Full technical architecture
│   ├── CVE-STRATEGY.md        # Security hardening strategy
│   ├── API-SURFACE.md         # Kernel API reference
│   ├── SANDBOX.md             # Sandbox system design
│   ├── SERVICE-FORMAT.md      # Service file format specification
│   └── CLI.md                 # CLI design and output formats
├── pid1/
│   └── main.c                 # PID 1 stub (141 lines, C, static)
├── src/
│   ├── main.rs                # Startup, io_uring loop, system dispatch
│   ├── sys.rs                 # ALL unsafe — syscall wrappers
│   ├── world.rs               # World struct, ServiceId, component storage
│   ├── components.rs          # Component type definitions
│   ├── error.rs               # SupervisorError enum
│   ├── ring.rs                # io_uring tag encoding + helpers
│   ├── config.rs              # Parse /etc/sv/ into components
│   ├── sandbox.rs             # Landlock + seccomp + capabilities
│   ├── cgroup.rs              # cgroup v2 resource limits + PSI
│   └── systems/
│       ├── mod.rs
│       ├── spawn.rs           # clone3 + namespace + socket activation
│       ├── supervise.rs       # Process exit handling + restart backoff
│       ├── deps.rs            # Dependency resolver (fanotify)
│       ├── log.rs             # Log routing (pipe → file + rotation)
│       ├── socket.rs          # Socket activation (sd_listen_fds)
│       ├── mounts.rs          # Mount namespace setup
│       ├── pressure.rs        # PSI event handler
│       └── status.rs          # Status file writer (stub)
├── ark/                       # CLI binary (no io_uring dependency)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs
│       └── cli.rs
├── Makefile                   # make build/test/install/clean/check/fmt
├── rust-toolchain.toml        # Pin stable + clippy + rustfmt
├── .cargo/config.toml         # Static linking flags
└── BUILDING.md                # Build documentation
```

## Build and test

```bash
make build            # cargo build --release + cc -static pid1/main.c
make test             # cargo test (46 tests, some need root)
make install          # to /usr/lib/arkhe/arkhd, /usr/bin/ark, /usr/sbin/pid1
make check            # cargo clippy -- -D warnings
```

See `BUILDING.md` for full details including static linking and musl targets.

## Performance targets

| Metric | Target |
|---|---|
| Boot to ready (50 services) | <1 second |
| PID 1 memory | <1 MB |
| Total init stack memory | <4 MB |
| Per-service launch overhead | <500μs (including sandbox setup) |
| Sandbox setup cost | <200μs per service |
| Seccomp filter overhead | <50ns per syscall |

## Code quality rules

1. No `unsafe` outside `src/sys.rs`. Every syscall goes through sys.rs safe wrappers.
2. No panics in PID 1. PID 1 does not allocate.
3. Every public function has a doc comment.
4. Every error is handled explicitly. No `.unwrap()` in production code.
5. Plain text config parsing is done with string splitting. No parser combinators, no grammars.
6. The entire codebase stays under 10K lines. If it's growing past that, something is wrong architecturally.

## Build priorities — ALL COMPLETE

| # | System | Status | Lines |
|---|--------|--------|-------|
| 1 | PID 1 stub | Done | 141 (C) |
| 2 | World + Components | Done | 307 |
| 3 | Config parser | Done | 745 |
| 4 | Service spawner (clone3) | Done | 284 |
| 5 | Dependency resolver (fanotify + polling fallback) | Done | 313 |
| 6 | Sandbox (Landlock + seccomp + caps + ABI degradation) | Done | 277 |
| 7 | Log router (pipe → file + rotation) | Done | 183 |
| 8 | Socket activator (sd_listen_fds) | Done | 124 |
| 9 | Cgroup resource limits + PSI | Done | 181 |
| 10 | CLI (ark status/log/check/new) | Done | 488 |
| 11 | Mount namespace + ID-mapped mount wrappers | Done | 88 |
| 12 | Reproducible build (Makefile + static) | Done | — |
| — | Supervision (restart + backoff) | Done | 188 |
| — | io_uring event loop + ring tags | Done | 388 |
| — | sys.rs unsafe boundary | Done | 985 |

**Total: ~4,768 lines** (budget: 10,000). 46 tests. 3 static binaries.

### Binary sizes (static, release, x86_64)
| Binary | Size | Description |
|--------|------|-------------|
| pid1 | 761 KB | PID 1 stub (C, fully static) |
| arkhd | 3.1 MB | Supervisor (Rust, static-pie) |
| ark | 2.7 MB | CLI (Rust, static-pie) |
