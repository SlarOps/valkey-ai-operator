#!/bin/bash
# Args: $1=cluster_ip:port
set -e
CLUSTER_IP="$1"
echo "Rebalancing cluster via $CLUSTER_IP"
valkey-cli --cluster rebalance $CLUSTER_IP --cluster-use-empty-masters
echo "Rebalance complete. Cluster info:"
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO
