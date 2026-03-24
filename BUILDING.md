# Building arkhe

## Requirements

- **Rust**: stable toolchain (pinned via `rust-toolchain.toml`)
- **Linux kernel**: 6.12+ (io_uring, clone3, pidfd, Landlock ABI v6, fanotify)
- **C compiler**: for the PID 1 stub (static linking: `cc -static`)
- **Architecture**: x86_64 or aarch64

## Quick start

```bash
make build        # builds arkhd, ark, and pid1
make test         # runs all tests (some need root)
make install      # installs to /usr/lib/arkhe/arkhd, /usr/bin/ark, /usr/sbin/pid1
```

## Build details

### Supervisor (`arkhd`)

```bash
cargo build --release
```

Output: `target/release/arkhd` (~2 MB, statically linked via `.cargo/config.toml`)

### CLI (`ark`)

```bash
cargo build --release -p ark
```

Output: `target/release/ark` (~2 MB)

### PID 1 stub (`pid1`)

```bash
cc -static -O2 -Wall -Wextra -Werror -o pid1/pid1 pid1/main.c
```

Output: `pid1/pid1` (~800 KB, fully static, zero dependencies)

## Testing

```bash
cargo test                  # all unit tests (no root needed)
sudo cargo test             # includes integration tests (cgroup, namespace)
```

Tests that require root or specific kernel features skip gracefully
with a message when run without privileges.

## Static linking

The `.cargo/config.toml` sets `target-feature=+crt-static` for Linux targets,
producing fully static binaries. This is required because:

1. PID 1 starts before any shared libraries are available
2. The supervisor must be self-contained for reliability
3. Chimera Linux (downstream) uses musl and expects static init binaries

For musl targets, use:
```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

## Verification

After building, verify the binaries are statically linked:

```bash
file target/release/arkhd   # should say "statically linked"
file target/release/ark
file pid1/pid1               # should say "statically linked"
```

Generate checksums for reproducibility:

```bash
sha256sum target/release/arkhd target/release/ark pid1/pid1
```

## Code quality

```bash
make check    # cargo clippy -- -D warnings
make fmt      # cargo fmt --check
```
