# arkhe — CLI Design

## Binary name: `ark`

Short, fast to type, memorable. Three letters.

## Commands

### ark status

Overview of all services.

```
$ ark status
SERVICE        STATE     UPTIME    SANDBOX    READY    PID
networking     running   3h12m     strict     yes      412
dns            running   3h12m     strict     yes      418
nginx          running   3h11m     strict     yes      425
postgres       running   3h11m     permissive yes      431
myapp          running   2h04m     strict     yes      502
failedthing    failing   -         strict     no       -
```

With `--verbose` or `-v`:
```
$ ark status nginx
SERVICE: nginx
  state:     running
  pid:       425
  uptime:    3h11m
  sandbox:   strict
  ready:     yes
  cgroup:    /sys/fs/cgroup/arkhe.slice/nginx.scope
  memory:    42M / 512M (8%)
  cpu:       1.2%
  pids:      12 / 64
  restarts:  0
  log:       /var/log/sv/nginx/current (234K)
  depends:   network-online ✓, tls-certs ✓
  listen:    tcp:80, tcp:443
```

### ark start <service>

Start a service manually.

```
$ ark start nginx
nginx: started (pid 425)
```

### ark stop <service>

Stop a service. Sends SIGTERM, waits grace period, then SIGKILL.

```
$ ark stop nginx
nginx: stopping...
nginx: stopped (exit 0)
```

### ark restart <service>

Stop then start.

```
$ ark restart nginx
nginx: stopping...
nginx: stopped (exit 0)
nginx: started (pid 512)
```

### ark log <service>

Tail the service log.

```
$ ark log nginx
2026-03-20 14:23:01 [notice] 425#425: using the "epoll" event method
2026-03-20 14:23:01 [notice] 425#425: start worker processes
2026-03-20 14:23:01 [notice] 425#425: start worker process 426
```

Options:
- `-f` / `--follow` — follow (like tail -f)
- `--since <duration>` — show logs since duration ago (e.g., `--since 1h`)
- `--lines <n>` / `-n <n>` — show last N lines (default 20)

Implementation: these are thin wrappers around `tail` and `grep` on plain text files. The CLI adds no value over the raw files — it's convenience, not necessity. This is the pronoic test: if `ark` disappears, you still have your logs.

### ark check

Audit the system. Plain-language diagnostics.

```
$ ark check
PERMISSIVE SERVICES (1):
  postgres — sandbox=permissive (reason: "needs shared memory IPC")

UNHEALTHY SERVICES (1):
  failedthing — exit code 1, restarted 3 times in 10 minutes
  last log: "connection refused: localhost:6379"
  likely cause: depends on redis, which is not defined as a service

RESOURCE PRESSURE (1):
  myapp — memory pressure: 12% stall time in last 60s
  current: 380M / 512M
  suggestion: increase memory-max or investigate leak

DEPENDENCY ISSUES (0):
  no circular dependencies detected

NO ISSUES (3): networking, dns, nginx

BOOT TIME: 0.847s (50 services)
```

This is the key pronoic UX feature. The system tells you what's wrong in sentences, not error codes. It diagnoses probable causes. It suggests fixes.

### ark new <service>

Scaffold a new service directory.

```
$ ark new myapp
Created /etc/sv/myapp/
  run      — edit this to start your service
  sandbox  — strict defaults (edit only to weaken)
```

### ark enable <service>

Enable a service to start at boot (creates a symlink or marker).

```
$ ark enable nginx
nginx: enabled (will start at boot)
```

### ark disable <service>

Disable a service from starting at boot.

```
$ ark disable nginx
nginx: disabled (will not start at boot, currently running)
```

## Implementation

The CLI is a separate binary from the supervisor. It reads plain text files from `/run/arkhe/` and `/etc/sv/`. It does NOT communicate with the supervisor via IPC.

For commands that change state (start, stop, restart, enable, disable), the CLI writes to a control directory (e.g., `/run/arkhe/ctl/`) which the supervisor watches via inotify. Or it sends signals:

```
# To reload service definitions:
kill -HUP $(cat /run/arkhe/supervisor.pid)
```

The CLI is unprivileged for read operations (status, log, check). Write operations (start, stop) require root or membership in an `arkhe` group.

## Output format

Default: human-readable plain text (as shown above).

`--json` flag: machine-readable JSON output for scripting.

```
$ ark status --json
[
  {"service": "nginx", "state": "running", "uptime": 11520, "sandbox": "strict", "ready": true, "pid": 425},
  ...
]
```

No other formats. Two is enough.

## Exit codes

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | General error |
| 2 | Service not found |
| 3 | Service failed to start |
| 4 | Permission denied |

## No features

The CLI does NOT:
- Have interactive mode
- Have a TUI
- Support plugins
- Have shell completion (can be added later as a separate script)
- Colorize output by default (respects `NO_COLOR` env var; add `--color` flag)
