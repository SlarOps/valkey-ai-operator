#!/bin/bash
# Args: $1=pod_ips (comma-separated host:port), $2=replicas_per_master
set -e
POD_IPS="$1"
REPLICAS="${2:-1}"
echo "Creating cluster with endpoints: $POD_IPS, replicas_per_master: $REPLICAS"
valkey-cli --cluster create $POD_IPS --cluster-replicas $REPLICAS --cluster-yes
echo "Cluster created. Verifying..."
sleep 2
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO
