#!/bin/bash
# Env vars: POD_IPS (comma or space separated host:port), REPLICAS_PER_MASTER
set -e
POD_IPS="${POD_IPS:?POD_IPS is required}"
# Normalize: convert commas to spaces
POD_IPS="${POD_IPS//,/ }"
REPLICAS="${REPLICAS_PER_MASTER:-1}"
echo "Creating cluster with endpoints: $POD_IPS, replicas_per_master: $REPLICAS"
valkey-cli --cluster create $POD_IPS --cluster-replicas $REPLICAS --cluster-yes
echo "Cluster created. Verifying..."
sleep 2
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO
