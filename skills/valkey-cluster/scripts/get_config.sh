#!/bin/bash
# Returns key Valkey runtime configuration
set -e
PORT="${VALKEY_PORT:-6379}"
echo "=== Memory ==="
valkey-cli -p $PORT CONFIG GET maxmemory
valkey-cli -p $PORT CONFIG GET maxmemory-policy
echo "=== Info Memory ==="
valkey-cli -p $PORT INFO memory | grep -E "used_memory_human|maxmemory_human|maxmemory_policy"
echo "=== Cluster ==="
valkey-cli -p $PORT CONFIG GET cluster-enabled
