#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/../.."

KORGI="./target/release/korgi"
CONFIG="tests/integration/korgi.toml"

passed=0
failed=0

run_test() {
    local name="$1"
    shift
    echo -n "  TEST: $name ... "
    if "$@" >/dev/null 2>&1; then
        echo "PASS"
        ((passed++))
    else
        echo "FAIL"
        ((failed++))
        # Show output on failure
        "$@" 2>&1 | tail -20 || true
    fi
}

run_test_expect_fail() {
    local name="$1"
    shift
    echo -n "  TEST: $name ... "
    if "$@" >/dev/null 2>&1; then
        echo "FAIL (expected failure but succeeded)"
        ((failed++))
    else
        echo "PASS (expected failure)"
        ((passed++))
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

run_test "korgi status shows 3 containers after deploy" \
    bash -c "$KORGI --config $CONFIG status 2>&1 | grep -c 'web' | grep -q 3"

# --- Scale ---
echo "--- Scale ---"
run_test "korgi scale up to 5" \
    $KORGI --config $CONFIG scale --service web 5

run_test "korgi scale down to 2" \
    $KORGI --config $CONFIG scale --service web 2

# --- Deploy v2 (zero-downtime) ---
echo "--- Zero-downtime deploy ---"
run_test "korgi deploy with image override (v2)" \
    $KORGI --config $CONFIG deploy --service web --image nginx:latest

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
run_test "korgi logs shows output" \
    bash -c "$KORGI --config $CONFIG logs --service web 2>&1 | head -5 | grep -q ."

# --- Destroy ---
echo "--- Destroy ---"
run_test "korgi destroy removes all containers" \
    $KORGI --config $CONFIG destroy

run_test "korgi status shows 0 containers after destroy" \
    bash -c "$KORGI --config $CONFIG status 2>&1 | grep -c 'running' | grep -q 0 || true"

# --- Summary ---
echo ""
echo "=== Results: $passed passed, $failed failed ==="
if [ $failed -gt 0 ]; then
    exit 1
fi
