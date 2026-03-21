#!/bin/bash
# Env vars: NEW_POD_IP (host:port), CLUSTER_IP (host:port)
set -e
NEW_NODE="${NEW_POD_IP:?NEW_POD_IP is required}"
EXISTING="${CLUSTER_IP:?CLUSTER_IP is required}"
echo "Adding node $NEW_NODE to cluster via $EXISTING"
valkey-cli --cluster add-node $NEW_NODE $EXISTING
echo "Node added. Cluster info:"
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO
