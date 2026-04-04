#!/bin/bash
set -uo pipefail
cd "$(dirname "$0")/../.."

KORGI="./target/release/korgi"
CONFIG="tests/integration/korgi.toml"

passed=0
failed=0

run_test() {
    local name="$1"
    shift
    echo -n "  TEST: $name ... "
    if timeout 120 "$@" >/dev/null 2>&1; then
        echo "PASS"
        passed=$((passed + 1))
    else
        echo "FAIL"
        failed=$((failed + 1))
        # Show output on failure
        timeout 120 "$@" 2>&1 | tail -20 || true
    fi
}

echo "=== Korgi Integration Tests ==="
echo ""

# Ensure binary is built
if [ ! -f "$KORGI" ]; then
    echo "Building korgi..."
    cargo build --release
fi

# --- Check ---
echo "--- Check ---"
run_test "korgi check validates config and connectivity" \
    $KORGI --config $CONFIG check

# --- Status (empty) ---
echo "--- Status ---"
run_test "korgi status works with no containers" \
    $KORGI --config $CONFIG status

# --- Traefik Deploy ---
echo "--- Traefik ---"
run_test "korgi traefik deploy" \
    $KORGI --config $CONFIG traefik deploy

run_test "korgi traefik status shows running" \
    $KORGI --config $CONFIG traefik status

# --- Deploy ---
echo "--- Deploy ---"
run_test "korgi deploy (first deploy)" \
    $KORGI --config $CONFIG deploy

echo -n "  TEST: korgi status shows containers after deploy ... "
STATUS_OUT=$(timeout 30 $KORGI --config $CONFIG status 2>&1 || true)
CONTAINER_COUNT=$(echo "$STATUS_OUT" | grep -c "web" || true)
if [ "$CONTAINER_COUNT" -ge 3 ]; then
    echo "PASS ($CONTAINER_COUNT containers)"
    passed=$((passed + 1))
else
    echo "FAIL (expected >= 3, got $CONTAINER_COUNT)"
    failed=$((failed + 1))
    echo "$STATUS_OUT" | tail -10
fi

# --- Scale ---
echo "--- Scale ---"
run_test "korgi scale up to 5" \
    $KORGI --config $CONFIG scale --service web 5

run_test "korgi scale down to 2" \
    $KORGI --config $CONFIG scale --service web 2

# --- Deploy v2 (zero-downtime) ---
echo "--- Zero-downtime deploy ---"
run_test "korgi deploy with image override (v2)" \
    $KORGI --config $CONFIG deploy --service web --image nginx:1.27-alpine

# --- Rollback ---
echo "--- Rollback ---"
run_test "korgi rollback" \
    $KORGI --config $CONFIG rollback --service web

# --- Exec ---
echo "--- Exec ---"
run_test "korgi exec runs command in container" \
    $KORGI --config $CONFIG exec --service web -- echo hello

# --- Logs ---
echo "--- Logs ---"
echo -n "  TEST: korgi logs shows output ... "
LOGS_OUT=$(timeout 10 $KORGI --config $CONFIG logs --service web 2>&1 || true)
if [ -n "$LOGS_OUT" ]; then
    echo "PASS"
    passed=$((passed + 1))
else
    echo "FAIL (empty output)"
    failed=$((failed + 1))
fi

# --- Destroy ---
echo "--- Destroy ---"
run_test "korgi destroy removes all containers" \
    $KORGI --config $CONFIG destroy

echo -n "  TEST: korgi status shows 0 containers after destroy ... "
STATUS_AFTER=$(timeout 30 $KORGI --config $CONFIG status 2>&1 || true)
if echo "$STATUS_AFTER" | grep -q "No containers"; then
    echo "PASS"
    passed=$((passed + 1))
else
    echo "FAIL"
    failed=$((failed + 1))
    echo "$STATUS_AFTER" | tail -5
fi

# --- Summary ---
echo ""
echo "=== Results: $passed passed, $failed failed ==="
if [ "$failed" -gt 0 ]; then
    exit 1
fi
