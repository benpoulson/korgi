#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"

echo "=== Korgi Integration Test Setup ==="

# Generate test SSH key if not present
if [ ! -f test_key ]; then
    echo "Generating test SSH key..."
    ssh-keygen -t ed25519 -f test_key -N "" -q
fi

# Build and start hosts
echo "Starting test hosts..."
docker compose up -d --build

# Wait for SSH to be ready on both hosts
echo "Waiting for SSH..."
for port in 2201 2202; do
    for i in $(seq 1 30); do
        if ssh -o StrictHostKeyChecking=no -o ConnectTimeout=2 -i test_key -p $port root@127.0.0.1 "echo ok" >/dev/null 2>&1; then
            echo "  Host on port $port: SSH ready"
            break
        fi
        if [ $i -eq 30 ]; then
            echo "  Host on port $port: SSH not ready after 30s"
            exit 1
        fi
        sleep 1
    done
done

# Wait for Docker to be ready inside hosts
echo "Waiting for Docker..."
for port in 2201 2202; do
    for i in $(seq 1 30); do
        if ssh -o StrictHostKeyChecking=no -i test_key -p $port root@127.0.0.1 "docker info" >/dev/null 2>&1; then
            echo "  Host on port $port: Docker ready"
            break
        fi
        if [ $i -eq 30 ]; then
            echo "  Host on port $port: Docker not ready after 30s"
            exit 1
        fi
        sleep 1
    done
done

echo ""
echo "=== Test environment ready ==="
echo "  host-a: ssh -i test_key -p 2201 root@127.0.0.1"
echo "  host-b: ssh -i test_key -p 2202 root@127.0.0.1"
