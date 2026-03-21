#!/bin/bash
# Move all slots from one node to another
# Env vars: FROM_NODE_ID, TO_NODE_ID, CLUSTER_IP (host:port)
set -e
FROM_NODE_ID="${FROM_NODE_ID:?FROM_NODE_ID is required}"
TO_NODE_ID="${TO_NODE_ID:?TO_NODE_ID is required}"
CLUSTER_IP="${CLUSTER_IP:?CLUSTER_IP is required}"

echo "Resharding: moving all slots from $FROM_NODE_ID to $TO_NODE_ID via $CLUSTER_IP"
valkey-cli --cluster reshard $CLUSTER_IP \
  --cluster-from $FROM_NODE_ID \
  --cluster-to $TO_NODE_ID \
  --cluster-slots 16384 \
  --cluster-yes
echo "Reshard complete. Cluster info:"
valkey-cli -h $(echo $CLUSTER_IP | cut -d: -f1) -p $(echo $CLUSTER_IP | cut -d: -f2) CLUSTER INFO
