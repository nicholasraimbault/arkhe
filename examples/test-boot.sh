#!/bin/bash
# Smoke test for arkhe — run as root on a Linux system.
# This is NOT a real PID 1 boot test. It runs the supervisor in the
# foreground to verify services are spawned, logged, and manageable.

set -e

ARKHE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ARKHD="$ARKHE_DIR/target/release/arkhd"
ARK="$ARKHE_DIR/target/release/ark"

echo "=== arkhe smoke test ==="
echo "supervisor: $ARKHD"
echo "cli:        $ARK"

# Check binaries exist
if [ ! -x "$ARKHD" ] || [ ! -x "$ARK" ]; then
    echo "ERROR: build first with 'make build'"
    exit 1
fi

# Set up directories
echo ""
echo "--- Setting up directories ---"
mkdir -p /etc/sv /run/ready /run/arkhe /var/log/arkhe

# Copy example services (only simple ones for smoke test)
echo "--- Copying example services ---"
for svc in network-online syslogd crond; do
    if [ -d "$ARKHE_DIR/examples/services/$svc" ]; then
        rm -rf "/etc/sv/$svc"
        cp -r "$ARKHE_DIR/examples/services/$svc" /etc/sv/
        chmod +x "/etc/sv/$svc/run"
        echo "  copied $svc"
    fi
done

# Clean previous state
rm -rf /run/arkhe/* /run/ready/*

# Start supervisor in background
echo ""
echo "--- Starting supervisor ---"
"$ARKHD" &
ARKHD_PID=$!
echo "supervisor PID: $ARKHD_PID"

# Wait for supervisor to initialize
sleep 2

# Show status
echo ""
echo "--- Service status (after 2s) ---"
"$ARK" status || true

# Wait for network-online to signal readiness
echo ""
echo "--- Waiting for network-online readiness ---"
for i in $(seq 1 10); do
    if [ -f /run/ready/network-online ]; then
        echo "network-online ready after ${i}s"
        break
    fi
    sleep 1
done

# Show status again — sshd should now be starting (dep satisfied)
echo ""
echo "--- Service status (after readiness) ---"
"$ARK" status || true

# Check configs
echo ""
echo "--- Config check ---"
"$ARK" check || true

# Show logs
echo ""
echo "--- Logs ---"
for svc in network-online syslogd crond; do
    if [ -f "/var/log/arkhe/$svc/current" ]; then
        echo "[$svc]"
        "$ARK" log "$svc" -n 5 || true
        echo ""
    fi
done

# Test stop/start
echo ""
echo "--- Testing ark stop/start ---"
"$ARK" stop syslogd || true
sleep 1
"$ARK" status syslogd || true
echo ""
"$ARK" start syslogd || true
sleep 2
"$ARK" status syslogd || true

# Shut down
echo ""
echo "--- Stopping supervisor ---"
kill -TERM "$ARKHD_PID" 2>/dev/null || true
# Wait up to 3 seconds for clean exit, then force kill
for i in 1 2 3; do
    kill -0 "$ARKHD_PID" 2>/dev/null || break
    sleep 1
done
kill -9 "$ARKHD_PID" 2>/dev/null || true
wait "$ARKHD_PID" 2>/dev/null || true
echo "supervisor exited"

# Clean up
echo ""
echo "--- Cleaning up ---"
rm -rf /run/arkhe/* /run/ready/*
echo "done"
