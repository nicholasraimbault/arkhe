# arkhe — Kernel API Surface

## Minimum kernel version: 6.12+

For the "every modern feature, no compromises" target. This gives us Landlock ABI v6 (IPC scoping), all io_uring features, pidfd, clone3, everything.

Compatible with: Fedora 41+, Ubuntu 25.04+, any rolling release from late 2024.

## Process management

### clone3 + CLONE_PIDFD + CLONE_INTO_CGROUP

```c
struct clone_args args = {
    .flags = CLONE_PIDFD | CLONE_INTO_CGROUP | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWIPC | CLONE_NEWUTS,
    .pidfd = &pidfd,           // output: pidfd for the child
    .child_tid = 0,
    .parent_tid = 0,
    .exit_signal = SIGCHLD,
    .cgroup = cgroup_fd,       // fd to pre-created cgroup directory
};
pid_t child = syscall(SYS_clone3, &args, sizeof(args));
```

- Returns pidfd via `args.pidfd` — race-free process reference
- `CLONE_INTO_CGROUP` places child directly into specified cgroup — atomic, no race window
- Namespace flags create isolated namespaces for the child
- Available since: kernel 5.7 (CLONE_INTO_CGROUP), 5.4 (CLONE_PIDFD), 5.3 (clone3)
- Rust: use `rustix::process` or direct `libc::syscall`

### pidfd_open / pidfd_send_signal

```c
int pidfd = pidfd_open(pid, 0);                    // get pidfd for existing process
pidfd_send_signal(pidfd, SIGTERM, NULL, 0);         // send signal via pidfd (race-free)
```

- Available since: kernel 5.2 (pidfd_open), 5.1 (pidfd_send_signal)

### waitid with P_PIDFD

```c
siginfo_t info;
waitid(P_PIDFD, pidfd, &info, WEXITED | WNOHANG);  // wait for process via pidfd
```

- Available since: kernel 5.4
- pidfds are also pollable: put in epoll/io_uring for exit notification

### close_range

```c
close_range(3, ~0U, CLOSE_RANGE_UNSHARE);  // close all fds >= 3
```

- Call before exec in the child to prevent fd leaks from supervisor
- Available since: kernel 5.9

## io_uring

### Setup

```c
struct io_uring ring;
io_uring_queue_init(64, &ring, 0);  // 64 entry ring
```

### Key operations used by arkhe

```
IORING_OP_POLL_ADD     — poll pidfds, inotify fd, signalfd (use multishot flag)
IORING_OP_ACCEPT       — socket activation (use multishot flag)
IORING_OP_SPLICE       — log routing (pipe → file, zero-copy in kernel)
IORING_OP_READ         — read from pipes/fds
IORING_OP_WRITE        — write to log files
IORING_OP_CLOSE        — close fds asynchronously
```

### Multishot operations

Multishot poll (kernel 5.13+): submit once, get completion on every event. Perfect for long-lived pidfd/inotify/signalfd monitoring. Set `IORING_POLL_ADD_MULTI` flag.

Multishot accept (kernel 5.19+): submit once on a listening socket, get a completion for each incoming connection. Perfect for socket activation.

### Linked SQEs

Chain read→write for log routing:
```
SQE1: IORING_OP_READ from service pipe → buffer
SQE2: IORING_OP_WRITE from buffer → log file (IOSQE_IO_LINK flag)
```

Or use SPLICE for zero-copy:
```
SQE: IORING_OP_SPLICE from pipe_fd → log_fd
```

## Sandboxing

### Landlock

```c
// Create ruleset
struct landlock_ruleset_attr attr = {
    .handled_access_fs = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_WRITE_FILE | ...,
    .handled_access_net = LANDLOCK_ACCESS_NET_BIND_TCP | LANDLOCK_ACCESS_NET_CONNECT_TCP,
    .scoped = LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET | LANDLOCK_SCOPE_SIGNAL,
};
int ruleset_fd = landlock_create_ruleset(&attr, sizeof(attr), 0);

// Add filesystem rules
struct landlock_path_beneath_attr path_attr = {
    .allowed_access = LANDLOCK_ACCESS_FS_READ_FILE | LANDLOCK_ACCESS_FS_READ_DIR,
    .parent_fd = open("/usr", O_PATH),
};
landlock_add_rule(ruleset_fd, LANDLOCK_RULE_PATH_BENEATH, &path_attr, 0);

// Add network rules
struct landlock_net_port_attr net_attr = {
    .allowed_access = LANDLOCK_ACCESS_NET_BIND_TCP,
    .port = 80,
};
landlock_add_rule(ruleset_fd, LANDLOCK_RULE_NET_PORT, &net_attr, 0);

// Enforce
prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
landlock_restrict_self(ruleset_fd, 0);
```

- Filesystem: kernel 5.13+ (ABI v1)
- File reparenting: kernel 5.19+ (ABI v2)
- File truncation: kernel 6.2+ (ABI v3)
- Network (TCP bind/connect): kernel 6.7+ (ABI v4)
- IOCTL restrictions: kernel 6.10+ (ABI v5)
- IPC scoping (abstract unix sockets, signals): kernel 6.12+ (ABI v6)

### seccomp-bpf

```c
// Install seccomp filter
struct sock_fprog prog = { .len = N, .filter = filter };
prctl(PR_SET_NO_NEW_PRIVS, 1);
prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &prog);
```

arkhe provides a default filter based on systemd's @system-service syscall set. Services can customize via the sandbox file.

### Capabilities

```c
// Drop all capabilities
prctl(PR_SET_KEEPCAPS, 0);
// Set to only declared caps
cap_set_proc(caps);
prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
```

### Namespaces

Created via `clone3` flags:
- `CLONE_NEWPID` — isolated PID namespace (service is PID 1 inside)
- `CLONE_NEWNS` — isolated mount namespace
- `CLONE_NEWIPC` — isolated IPC namespace (SysV IPC, POSIX mqueues)
- `CLONE_NEWUTS` — isolated hostname
- `CLONE_NEWNET` — isolated network (when network-namespace=private)

## Mount operations

### New mount API

```c
int fs_fd = fsopen("tmpfs", 0);
fsconfig(fs_fd, FSCONFIG_SET_STRING, "mode", "1777", 0);
fsconfig(fs_fd, FSCONFIG_CMD_CREATE, NULL, NULL, 0);
int mnt_fd = fsmount(fs_fd, 0, 0);
move_mount(mnt_fd, "", AT_FDCWD, "/tmp", MOVE_MOUNT_F_EMPTY_PATH);
```

Used for setting up private /tmp and other mount namespace contents.

### ID-mapped mounts

```c
int tree_fd = open_tree(AT_FDCWD, "/srv/data", OPEN_TREE_CLONE);
struct mount_attr attr = {
    .attr_set = MOUNT_ATTR_IDMAP,
    .userns_fd = userns_fd,  // fd to user namespace with desired mapping
};
mount_setattr(tree_fd, "", AT_EMPTY_PATH | AT_RECURSIVE, &attr, sizeof(attr));
move_mount(tree_fd, "", AT_FDCWD, "/mnt/data", MOVE_MOUNT_F_EMPTY_PATH);
```

Allows services to see files with mapped uid/gid without changing on-disk ownership. Available since kernel 5.12.

## cgroup v2

### Create service cgroup

```bash
mkdir /sys/fs/cgroup/arkhe.slice/<service>.scope
```

### Set resource limits

```bash
echo "512M" > /sys/fs/cgroup/arkhe.slice/<service>.scope/memory.max
echo "100" > /sys/fs/cgroup/arkhe.slice/<service>.scope/cpu.weight
echo "64" > /sys/fs/cgroup/arkhe.slice/<service>.scope/pids.max
```

### PSI monitoring

```c
int fd = open("/sys/fs/cgroup/arkhe.slice/<service>.scope/memory.pressure", O_RDWR);
// Register trigger: notify when >10% of time spent stalled over 1 second window
write(fd, "some 100000 1000000", ...);  // threshold_us window_us
// fd is now an eventfd — add to io_uring ring
```

### Atomic cgroup placement

Use `CLONE_INTO_CGROUP` with clone3 (see Process management above). Open the cgroup directory as an fd and pass it in clone_args.cgroup.

## inotify

### Dependency resolution

```c
int inotify_fd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
inotify_add_watch(inotify_fd, "/run/ready", IN_CREATE | IN_DELETE);
// Add to io_uring ring via IORING_OP_POLL_ADD multishot
```

When a file is created in `/run/ready/`, the supervisor receives an inotify event with the filename. It checks if any waiting services have all dependencies satisfied.

## signalfd

```c
sigset_t mask;
sigemptyset(&mask);
sigaddset(&mask, SIGCHLD);
sigaddset(&mask, SIGTERM);
sigaddset(&mask, SIGHUP);
sigprocmask(SIG_BLOCK, &mask, NULL);
int sfd = signalfd(-1, &mask, SFD_NONBLOCK | SFD_CLOEXEC);
// Add to io_uring ring via IORING_OP_POLL_ADD multishot
```
