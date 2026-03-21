#!/bin/bash
# Env vars: CLUSTER_IP (host:port)
set -e
CLUSTER_IP="${CLUSTER_IP:?CLUSTER_IP is required}"
echo "Rebalancing cluster via $CLUSTER_IP"
valkey-cli --cluster rebalance $CLUSTER_IP --cluster-use-empty-masters
echo "Rebalance complete. Cluster info:"
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO
