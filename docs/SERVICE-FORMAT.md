# arkhe — Service File Format

## Overview

Each service is a directory under `/etc/sv/<name>/`. Configuration is one-concern-per-file, plain text, greppable, diffable. No INI parser, no TOML, no YAML, no custom DSL.

## Directory structure

```
/etc/sv/nginx/
├── run              # REQUIRED: executable that starts the service
├── finish           # OPTIONAL: cleanup script run after service exits
├── depends          # OPTIONAL: one dependency name per line
├── ready            # OPTIONAL: readiness configuration
├── env/             # OPTIONAL: environment variables as files
│   ├── LANG         # contains: C.UTF-8
│   └── NGINX_CONF   # contains: /etc/nginx/nginx.conf
├── sandbox          # OPTIONAL: sandbox overrides (defaults are strict)
├── resources        # OPTIONAL: cgroup resource limits
├── listen           # OPTIONAL: socket activation sockets
└── log/
    └── config       # OPTIONAL: log rotation config
```

## File specifications

### run (REQUIRED)

An executable file. Typically a shell script. The service process. Must NOT daemonize — must run in the foreground. arkhe supervises the process directly.

```bash
#!/bin/sh
exec nginx -g 'daemon off;'
```

Or for compiled services:
```bash
#!/bin/sh
exec /usr/bin/myapp --config /etc/myapp/config
```

The `run` script is executed inside the sandbox. By the time it runs, namespaces are set up, Landlock is applied, capabilities are dropped.

### finish (OPTIONAL)

Executed after the service exits, before restart. Receives exit code as $1 and signal number as $2. Useful for cleanup.

```bash
#!/bin/sh
# $1 = exit code, $2 = signal that killed it
rm -f /run/nginx.pid
```

### depends (OPTIONAL)

One dependency name per line. Service will not start until all dependencies have signaled readiness (files exist in /run/ready/).

```
network-online
dns-ready
tls-certs
```

Empty or absent: no dependencies, start immediately.

### ready (OPTIONAL)

Configures how the service signals readiness.

```
# One of:
mode = file          # (default) service creates /run/ready/<name> itself
mode = fd            # supervisor passes a notification fd
mode = timeout       # assume ready after N seconds
timeout = 30         # max seconds to wait for readiness (default 30)
```

If absent: mode=file, timeout=30.

### env/ (OPTIONAL)

Directory containing environment variables. Each file's name is the variable name. Each file's content (trimmed) is the value.

```
/etc/sv/nginx/env/LANG     → contents: "C.UTF-8"
/etc/sv/nginx/env/WORKERS  → contents: "4"
```

These are set in the service's environment before exec.

### sandbox (OPTIONAL)

Overrides for the default-deny sandbox. **If this file is absent, strict defaults apply.** You only create this file to WEAKEN the sandbox.

Format: `key = value`, one per line. Comments start with `#`.

```
# Landlock filesystem access
# Default: deny all
# Specify paths the service CAN access
read = /usr, /lib, /lib64, /etc/nginx, /etc/ssl/certs
write = /var/log/nginx, /run/nginx
exec = /usr/sbin/nginx, /usr/bin/sh

# Landlock network access
# Default: deny all
# Specify ports the service CAN use
bind = 80, 443
connect = none

# Namespace isolation (all default to yes)
pid-namespace = yes
mount-namespace = yes
ipc-namespace = yes
uts-namespace = yes
# Set to 'host' to share host network (required for network services)
network-namespace = host

# Private tmp (default: yes)
private-tmp = yes

# Read-only root (default: yes)
read-only-root = yes

# Capabilities (default: none)
# Only list caps the service NEEDS
caps = net_bind_service

# Seccomp filter (default: @system-service whitelist)
seccomp = default

# Landlock IPC scoping (default: scoped)
# Restricts abstract unix sockets and signals to same domain
ipc-scope = scoped

# ESCAPE HATCH: set to 'permissive' to disable sandboxing
# This is LOGGED and VISIBLE in `ark check`
# sandbox = permissive
```

**Default values when sandbox file is absent or a key is missing:**

| Key | Default |
|---|---|
| read | deny all |
| write | deny all |
| exec | deny all |
| bind | deny all |
| connect | deny all |
| pid-namespace | yes |
| mount-namespace | yes |
| ipc-namespace | yes |
| uts-namespace | yes |
| network-namespace | private |
| private-tmp | yes |
| read-only-root | yes |
| caps | none |
| seccomp | default (@system-service) |
| ipc-scope | scoped |

### resources (OPTIONAL)

cgroup v2 resource limits. Written to cgroup interface files before the service starts.

```
memory-max = 512M
memory-high = 384M
cpu-weight = 100
cpu-max = 80000 100000
io-weight = 100
pids-max = 64
```

If absent: no resource limits (cgroup is still created for accounting and PSI).

### listen (OPTIONAL)

Socket activation configuration. One socket per line.

```
tcp:80
tcp:443
tcp6:[::]:8080
unix:/run/nginx.sock
```

Sockets are bound by the supervisor at boot. File descriptors are passed to the service on first connection.

### log/config (OPTIONAL)

Log rotation configuration.

```
max-size = 1M
max-files = 10
```

Defaults: 1MB max size, 10 rotated files.

## Scaffold command

`ark new <name>` creates a service directory with all defaults:

```bash
$ ark new myapp
Created /etc/sv/myapp/
  run      — edit this to start your service
  sandbox  — pre-filled with strict defaults (edit to weaken)
```

The scaffold `run` file:
```bash
#!/bin/sh
# Edit this file to start your service.
# The command must run in the foreground (no daemonizing).
exec /path/to/your/binary
```

The scaffold `sandbox` file contains all defaults as comments, documenting every option. The user uncomments and modifies only what they need to weaken.

## Parsing rules

The supervisor parses these files with minimal logic:
- Read file as string
- Split on newlines
- Skip lines starting with `#`
- Split on first `=` for key-value pairs
- Trim whitespace
- Split on `,` for list values

No parser library. No grammar. No error recovery. If a file is malformed, the service fails to load and `ark check` reports the error in plain language.
