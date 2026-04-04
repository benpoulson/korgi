#!/bin/bash
set -e

# Start Docker daemon in background
dockerd-entrypoint.sh &

# Wait for Docker to be ready
for i in $(seq 1 30); do
    if docker info >/dev/null 2>&1; then
        echo "Docker daemon ready"
        break
    fi
    sleep 1
done

# Start SSH daemon in foreground
exec /usr/sbin/sshd -D -e
