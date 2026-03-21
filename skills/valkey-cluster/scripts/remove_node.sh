#!/bin/bash
# Remove a node from the cluster (must have no slots assigned)
# Env vars: NODE_ID, CLUSTER_IP (host:port)
set -e
NODE_ID="${NODE_ID:?NODE_ID is required}"
CLUSTER_IP="${CLUSTER_IP:?CLUSTER_IP is required}"

echo "Removing node $NODE_ID from cluster via $CLUSTER_IP"
valkey-cli --cluster del-node $CLUSTER_IP $NODE_ID
echo "Node removed. Cluster info:"
valkey-cli -h $(echo $CLUSTER_IP | cut -d: -f1) -p $(echo $CLUSTER_IP | cut -d: -f2) CLUSTER INFO
