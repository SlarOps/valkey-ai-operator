---
name: valkey-cluster
description: Manage Valkey cluster on Kubernetes. Handles creation, scaling, self-healing, and resource management for Valkey distributed clusters.
allowed-tools: run_action, apply_template, get_state, update_status, get_pod_logs, wait_for_ready, get_events

monitors:
  - name: cluster_state
    interval: 30s
    script: scripts/monitors/cluster_info.sh
    parse: key-value
    trigger_when: "cluster_state != ok"
  - name: pod_health
    interval: 10s
    script: scripts/monitors/health_check.sh
    parse: exit-code
    trigger_when: "exit_code != 0"

actions:
  - name: cluster_init
    risk: high
    description: Create new Valkey cluster from scratch
    script: scripts/cluster_init.sh
    params: [pod_ips, replicas_per_master]
  - name: add_node
    risk: medium
    description: Add node to existing cluster
    script: scripts/add_node.sh
    params: [new_pod_ip, cluster_ip]
  - name: rebalance
    risk: medium
    description: Rebalance slots across masters
    script: scripts/rebalance.sh
    params: [cluster_ip]
  - name: health_check
    risk: low
    description: Check health of all nodes
    script: scripts/monitors/health_check.sh

agents:
  planner:
    system_prompt_file: prompts/planner.md
  simulator:
    system_prompt_file: prompts/simulator.md
  executor:
    system_prompt_file: prompts/executor.md
  verifier:
    system_prompt_file: prompts/verifier.md
---

# Valkey Cluster Knowledge

Valkey is a distributed key-value store (Redis-compatible) running in cluster mode.

## Cluster Topology
- A cluster has **masters** (hold data in 16384 hash slots) and **replicas** (failover copies)
- Total pods = masters + (masters × replicas_per_master)
- Each master owns a portion of the 16384 hash slots
- Replicas automatically failover if their master fails

## Creating a New Cluster
1. Apply statefulset.yaml template with total pod count
2. Apply service.yaml template for headless service (pod discovery)
3. Apply configmap.yaml template with cluster-enabled config
4. Wait for all pods to be ready
5. Run cluster_init action with all pod IPs and replicas_per_master
6. Verify cluster_state=ok via monitors

## Scaling Up (add masters)
1. Increase StatefulSet replicas
2. Wait for new pods to be ready
3. Run add_node action for each new pod
4. Run rebalance action to redistribute slots

## Scaling Down
- NOT supported in v1. Do not reduce replicas below initial count.

## Health Check
- `valkey-cli PING` on each node → expect PONG
- `CLUSTER INFO` → check cluster_state:ok, cluster_slots_ok:16384

## Healing
- **Pod crash**: K8s restarts automatically, Valkey auto-failover promotes replica
- **cluster_state:fail**: May need CLUSTER RESET HARD on affected nodes, then reinitialize
- **Slot migration stuck**: CLUSTER SETSLOT STABLE on affected slots

## Guardrails
- cluster_init: ONLY when cluster is empty, NEVER on existing data with slots assigned
- NEVER run: FLUSHALL, FLUSHDB, DEBUG, SHUTDOWN
- CONFIG SET: only maxmemory is allowed
- Always verify cluster_state before and after operations

## Port
- Default Valkey port: 6379
- Cluster bus port: 16379
