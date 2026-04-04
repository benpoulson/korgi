#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")"

echo "=== Tearing down test environment ==="
docker compose down -v --remove-orphans 2>/dev/null || true
echo "Done."
