---
name: valkey-cluster
description: Manage Valkey cluster on Kubernetes. Handles creation, scaling, self-healing, and resource management for Valkey distributed clusters.
allowed-tools: run_action, apply_template, get_state, update_status, get_pod_logs, wait_for_ready, get_events, kubectl_describe, kubectl_get, kubectl_scale, kubectl_patch, kubectl_exec

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
  - name: get_config
    risk: low
    description: Get Valkey runtime config (maxmemory, cluster-enabled, memory usage)
    script: scripts/get_config.sh

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
- Minimum: 3 masters (required by Valkey cluster protocol)

## Deployment Modes

### Standalone (single instance)
When goal says "a Valkey instance" or specifies 1 pod with no masters/replicas:
- replicas = 1
- cluster_enabled = **no** (IMPORTANT: must be "no" for standalone)
- Do NOT run cluster_init
- Skip cluster health checks

### Cluster mode
When goal mentions masters, replicas, or cluster:
- Minimum 3 masters required by Valkey cluster protocol
- replicas = masters + (masters × replicas_per_master)
- cluster_enabled = yes (default)

## Deployment Guide

### Step 1: Determine Mode and Pod Count
From the goal, determine standalone or cluster mode:
- "a Valkey instance" / "single instance" → **standalone**: 1 pod, cluster_enabled=no
- "N-master cluster with M replicas" → **cluster**: pods = N + (N × M), cluster_enabled=yes
- Example: "3-master with 1 replica each" → cluster, 3 + (3 × 1) = 6 pods

### Step 2: Apply Kubernetes Resources (in this order)
1. **configmap.yaml** — configuration
   - vars: `name`, `namespace`, `maxmemory`, `maxmemory_policy` (default: noeviction)
   - vars: `cluster_enabled` — set to "no" for standalone, "yes" for cluster
   - IMPORTANT: `maxmemory` must be in bytes (integer) or Valkey format (e.g. "1gb", "512mb"). Do NOT use Kubernetes format like "1Gi" or "512Mi". Conversion: 1Gi = 1073741824, 512Mi = 536870912, 2Gi = 2147483648
2. **service.yaml** — headless service for pod discovery
   - vars: `name`, `namespace`, `port` (default: 6379), `cluster_port` (default: 16379)
3. **statefulset.yaml** — the pods
   - vars: `name`, `namespace`, `image`, `replicas` (total pod count), `memory_limit`, `cpu_limit`, `storage`

### Step 3: Wait for All Pods Ready
- Use `wait_for_ready` with `expected_count` = total pods, `timeout_seconds` = 300
- All pods must be Running and Ready before proceeding

### Step 4: Initialize (cluster mode ONLY)
**Skip this step entirely for standalone mode.**
1. Call `get_state` to retrieve pod IPs
2. Build comma-separated list: `IP1:6379,IP2:6379,...`
3. IMPORTANT: Use actual pod IPs, NOT DNS names
4. Run `cluster_init` action on any pod (e.g., pod-0)
   - `pod_ips`: comma-separated `IP:PORT` list of ALL pods
   - `replicas_per_master`: number from goal (e.g., 1)

### Step 5: Verify
- Run `get_config` to verify maxmemory matches goal
- For standalone: check pod is Running and Ready, run health_check
- For cluster: verify cluster_state=ok, cluster_slots_ok=16384
- Update status to Running if healthy

## Scaling Up (add masters)
1. Increase StatefulSet replicas
2. Wait for new pods to be ready
3. Run add_node action for each new pod
4. Run rebalance action to redistribute slots

## Scaling Down
- NOT supported in v1. Do not reduce replicas below initial count.

## Spec Change Handling
When spec changes (e.g., memory increase):
1. Update configmap with new maxmemory value
2. Re-apply statefulset with new resource limits
3. Pods will rolling-restart automatically with new config
4. Verify cluster_state=ok after restart completes
- Do NOT re-run cluster_init on an existing cluster with data

## Health Check
- `valkey-cli PING` on each node → expect PONG
- `CLUSTER INFO` → check cluster_state:ok, cluster_slots_ok:16384

## Drift Healing
When the trigger source is "drift" and reason mentions missing resources:
1. The desired-state annotation on the AIResource contains the last-applied YAML for each template
2. Re-apply the missing templates using `apply_template` with the same variables as the original deployment
3. Use `get_state` to check current state and determine what variables to use
4. After re-applying, verify the resource is healthy (pods Running/Ready)
5. Update status accordingly

This is a deterministic operation — just re-apply what was there before. No need to re-plan the deployment.

## Healing
- **Pod crash**: K8s restarts automatically, Valkey auto-failover promotes replica
- **cluster_state:fail**: May need CLUSTER RESET HARD on affected nodes, then reinitialize
- **Slot migration stuck**: CLUSTER SETSLOT STABLE on affected slots

## Guardrails
- cluster_init: ONLY when cluster is empty (no slots assigned), NEVER on existing cluster with data
- NEVER run: FLUSHALL, FLUSHDB, DEBUG, SHUTDOWN
- CONFIG SET: only maxmemory is allowed
- Always verify cluster_state before and after operations

## Port
- Default Valkey port: 6379
- Cluster bus port: 16379

## Memory Format
Valkey uses its own memory format, NOT Kubernetes format:
- Valkey: `1gb`, `512mb`, `256mb` or bytes like `1073741824`
- Kubernetes: `1Gi`, `512Mi` (use this ONLY for container resource limits, NOT for maxmemory)
- When goal says "1Gi memory": use `1gb` for maxmemory config, `1Gi` for container resource limits
