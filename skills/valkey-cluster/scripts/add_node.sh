#!/bin/bash
# Args: $1=new_pod_ip:port, $2=existing_cluster_ip:port
set -e
NEW_NODE="$1"
EXISTING="$2"
echo "Adding node $NEW_NODE to cluster via $EXISTING"
valkey-cli --cluster add-node $NEW_NODE $EXISTING
echo "Node added. Cluster info:"
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO
