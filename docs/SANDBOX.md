# arkhe — Sandbox System Design

## Philosophy

Every service is sandboxed by default. There is no unsandboxed mode. The default is maximum restriction. Users weaken the sandbox explicitly, and every weakening is visible.

This inverts systemd's model where `systemd-analyze security` scores a default service at 9.6 UNSAFE and requires ~15 manual directives to reach safety. arkhe starts at the equivalent of 2.0 SAFE.

## Sandbox layers (applied in order during service spawn)

### Layer 1: cgroup (applied BEFORE clone)

1. Create cgroup directory: `/sys/fs/cgroup/arkhe.slice/<service>.scope/`
2. Enable controllers: `echo "+cpu +memory +io +pids" > cgroup.subtree_control`
3. Set resource limits from `/etc/sv/<service>/resources`
4. Register PSI eventfd triggers for memory/CPU pressure
5. Open cgroup directory as fd for `CLONE_INTO_CGROUP`

### Layer 2: clone3 (creates the child process)

```
clone3({
    .flags = CLONE_PIDFD | CLONE_INTO_CGROUP | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS [| CLONE_NEWNET],
    .pidfd = &pidfd,
    .cgroup = cgroup_fd,
    .exit_signal = SIGCHLD,
})
```

Child is now in:
- Its own PID namespace (sees itself as PID 1)
- Its own mount namespace (can modify mounts without affecting host)
- Its own IPC namespace (isolated SysV IPC)
- Its own UTS namespace (isolated hostname)
- Optionally its own network namespace
- Already inside its cgroup

### Layer 3: Mount namespace setup (in child, before exec)

Using the new mount API (`fsopen`/`fsmount`/`move_mount`):

1. **Read-only root:** Remount / as read-only via `mount_setattr` with `MOUNT_ATTR_RDONLY`
2. **Private /tmp:** Mount fresh tmpfs on /tmp
3. **Bind-mount writable paths:** For each path in `sandbox` `write = ...`, bind-mount with write access
4. **Bind-mount readable paths:** For each path in `sandbox` `read = ...`, bind-mount read-only
5. **ID-mapped mounts:** If service runs as a different uid, apply `MOUNT_ATTR_IDMAP` to relevant mounts
6. **Hide /proc processes:** Mount new procfs with `hidepid=2` (or use `ProtectProc=invisible` equivalent)

### Layer 4: close_range (prevent fd leaks)

```
close_range(3, ~0U, CLOSE_RANGE_UNSHARE);
```

Closes all file descriptors >= 3 except:
- Log pipe (stdout/stderr, fd 1 and 2)
- Readiness notification fd (if mode=fd)
- Socket activation fds (if applicable)

These are re-opened/dup'd to specific fd numbers before close_range.

### Layer 5: Landlock (filesystem + network + IPC scoping)

1. Create ruleset with `landlock_create_ruleset`:
   - `handled_access_fs` = all filesystem access types
   - `handled_access_net` = TCP bind + connect
   - `scoped` = abstract unix sockets + signals

2. Add allow rules from `sandbox` file:
   - For each path in `read`: add `LANDLOCK_ACCESS_FS_READ_FILE | READ_DIR`
   - For each path in `write`: add read + `LANDLOCK_ACCESS_FS_WRITE_FILE | REMOVE_FILE | MAKE_REG | ...`
   - For each path in `exec`: add `LANDLOCK_ACCESS_FS_EXECUTE`
   - For each port in `bind`: add `LANDLOCK_ACCESS_NET_BIND_TCP`
   - For each port in `connect`: add `LANDLOCK_ACCESS_NET_CONNECT_TCP`

3. If `ipc-scope = scoped` (default):
   - `LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET` — can't connect to abstract sockets outside domain
   - `LANDLOCK_SCOPE_SIGNAL` — can't send signals outside domain

4. Enforce: `prctl(PR_SET_NO_NEW_PRIVS, 1)` then `landlock_restrict_self(ruleset_fd, 0)`

### Layer 6: seccomp-bpf (syscall filtering)

Default filter: allow syscalls in the `@system-service` set (same set systemd uses for well-behaved services). Block dangerous syscalls like `kexec_load`, `mount`, `reboot`, `swapon`, `bpf`, etc.

Custom filters can be specified per-service in the `sandbox` file, but the default handles 95%+ of services.

BPF JIT compilation means ~10-50ns overhead per syscall at runtime.

### Layer 7: Capabilities (final privilege drop)

```
prctl(PR_SET_KEEPCAPS, 0);
// Set bounding set to only declared caps
// Clear ambient caps
// Set effective/permitted/inheritable to only declared caps
```

Default: no capabilities. Service declares what it needs in `sandbox` `caps = ...`.

### Layer 8: exec

The service binary is now running inside:
- PID namespace (isolated process view)
- Mount namespace (restricted filesystem view)
- IPC namespace (isolated IPC)
- cgroup (resource-limited)
- Landlock domain (filesystem + network + IPC access control)
- seccomp filter (syscall whitelist)
- No capabilities (except declared)
- No extra file descriptors

## Escape hatch: permissive mode

Setting `sandbox = permissive` in the sandbox file disables all sandboxing layers except the cgroup (resource limits always apply).

**This is loud:**
- `ark status` shows `sandbox=permissive` in the service line
- `ark check` lists all permissive services with a warning
- The supervisor logs a notice at boot: "service X running in permissive sandbox mode"
- The service's status file in `/run/arkhe/services/X` contains `sandbox=permissive`

The point: you CAN disable the sandbox. But you can't do it silently. The system always knows and always tells you.

## Default sandbox by service type

arkhe can auto-detect common service types and apply appropriate defaults:

| Service type | Detection | Additional defaults |
|---|---|---|
| Web server | `bind` in listen file | `network-namespace = host`, `caps = net_bind_service` |
| Database | writes to `/var/lib/` | `write += /var/lib/<service>`, larger memory-max |
| Background worker | no listen, no network | `network-namespace = private`, `connect = none` |

This is a future optimization, not required for v1. For v1, strict defaults + explicit weakening is sufficient.

## Testing sandboxes

`ark check` performs static analysis of sandbox configurations:

```
$ ark check
SERVICES (5):

  nginx
    sandbox: strict
    landlock: read=/usr,/lib,/etc/nginx,/etc/ssl  write=/var/log/nginx,/run/nginx  exec=/usr/sbin/nginx
    network: bind=80,443  connect=none
    caps: net_bind_service
    ✓ No issues

  postgres
    sandbox: PERMISSIVE ← review recommended
    reason: "needs shared memory IPC"
    ⚠ Running without sandbox restrictions

  myapp
    sandbox: strict
    landlock: read=/usr,/lib  write=none  exec=/usr/bin/myapp
    network: bind=none  connect=none
    caps: none
    ✓ No issues (fully isolated)

SUMMARY: 4 strict, 1 permissive
```
