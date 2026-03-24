# arkhe — pronoic init system
# Build, test, and install targets for the supervisor, CLI, and PID 1 stub.

.PHONY: build test install clean check fmt

RELEASE_DIR = target/release
INSTALL_BIN = /usr/bin
INSTALL_LIB = /usr/lib/arkhe
INSTALL_SBIN = /usr/sbin

CC ?= cc
CFLAGS ?= -static -O2 -Wall -Wextra -Werror

# Build all three binaries: arkhd (supervisor), ark (CLI), pid1 (init stub)
build:
	cargo build --release --workspace
	$(CC) $(CFLAGS) -o pid1/pid1 pid1/main.c

# Run all tests (some require root for cgroup/namespace operations)
test:
	cargo test

# Install binaries to system paths
install: build
	install -Dm755 $(RELEASE_DIR)/arkhd $(INSTALL_LIB)/arkhd
	install -Dm755 $(RELEASE_DIR)/ark $(INSTALL_BIN)/ark
	install -Dm755 pid1/pid1 $(INSTALL_SBIN)/pid1
	@echo "Installed: $(INSTALL_LIB)/arkhd $(INSTALL_BIN)/ark $(INSTALL_SBIN)/pid1"

# Remove build artifacts
clean:
	cargo clean
	rm -f pid1/pid1

# Run clippy with warnings as errors
check:
	cargo clippy -- -D warnings

# Check formatting
fmt:
	cargo fmt --check
