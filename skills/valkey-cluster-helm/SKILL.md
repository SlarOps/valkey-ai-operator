---
name: valkey-cluster-helm
description: Manage Valkey cluster on Kubernetes using the Bitnami Helm chart. Handles deployment, scaling, healing, and configuration changes.
chart: oci://registry-1.docker.io/bitnamicharts/valkey-cluster
allowed-tools: helm_install, helm_upgrade, helm_status, helm_get_values, helm_show_values, get_state, update_status, get_pod_logs, wait_for_ready, get_events, kubectl_describe, kubectl_get, kubectl_exec, file_read, ls, glob, grep, content_search, file_list

monitors:
  - name: pod_health
    interval: 10s
    script: scripts/monitors/health_check.sh
    parse: exit-code
    trigger_when: "exit_code != 0"

actions: []

agents:
  agent:
    system_prompt_file: prompts/agent.md
---

# Valkey Cluster via Bitnami Helm Chart

This skill manages Valkey clusters using the Bitnami `valkey-cluster` Helm chart (`oci://registry-1.docker.io/bitnamicharts/valkey-cluster`).

The chart deploys a **Valkey Cluster with sharding** — multiple write points using hash slot distribution across masters. Each master holds a portion of 16384 hash slots.

## Key Helm Values

| Value | Default | Description |
|-------|---------|-------------|
| `cluster.nodes` | `6` | Total node count. Must be >= 6. Formula: masters + (masters × replicas) |
| `cluster.replicas` | `1` | Replicas per master |
| `cluster.init` | `true` | Initialize cluster on first install. Set `false` on upgrades |
| `password` | `""` | Valkey password (required) |
| `persistence.enabled` | `true` | Enable persistent storage |
| `persistence.size` | `8Gi` | PVC size per node |
| `image.repository` | `docker.io/bitnami/valkey-cluster` | Container image |
| `image.tag` | `latest` | Image tag |
| `valkey.resources.limits.memory` | - | Memory limit per pod |
| `valkey.resources.limits.cpu` | - | CPU limit per pod |
| `valkey.resources.requests.memory` | - | Memory request per pod |
| `valkey.resources.requests.cpu` | - | CPU request per pod |

## Goal → Helm Values Mapping

Parse the AIResource goal to determine values:
- "3-master cluster with 1 replica each" → `cluster.nodes=6, cluster.replicas=1`
- "5-master cluster with 0 replicas" → `cluster.nodes=5` — **INVALID**, minimum 6 nodes
- "512mb memory" → `valkey.resources.limits.memory=512Mi, valkey.resources.requests.memory=512Mi`
- "1Gi memory" → `valkey.resources.limits.memory=1Gi, valkey.resources.requests.memory=1Gi`

## Image Handling — CRITICAL

**IMPORTANT**: The chart's default image on Docker Hub may not exist. The correct working image is on AWS ECR: `public.ecr.aws/bitnami/valkey-cluster:latest`.

**ALWAYS set these image values** (regardless of whether the goal mentions an image):
- `image.registry=public.ecr.aws`
- `image.repository=bitnami/valkey-cluster`
- `image.tag=latest`

**When the goal mentions a specific image**, parse it instead:
- `public.ecr.aws/bitnami/valkey-cluster:8.1` → `image.registry=public.ecr.aws, image.repository=bitnami/valkey-cluster, image.tag=8.1`
- `docker.io/custom/valkey:v1` → `image.registry=docker.io, image.repository=custom/valkey, image.tag=v1`

**NEVER** include the registry in `image.repository` — the chart prepends `image.registry/` automatically.
- CORRECT: `image.registry=public.ecr.aws, image.repository=bitnami/valkey-cluster, image.tag=latest`
- WRONG: `image.repository=public.ecr.aws/bitnami/valkey-cluster` (doubled registry)

## Bootstrap (first deploy)

```
helm install <release-name> oci://registry-1.docker.io/bitnamicharts/valkey-cluster \
  --namespace <ns> \
  --set cluster.nodes=6 \
  --set cluster.replicas=1 \
  --set password=<generated-or-from-secret> \
  --set persistence.size=8Gi \
  --set valkey.resources.limits.memory=512Mi \
  --set valkey.resources.requests.memory=512Mi \
  --wait --timeout 600s
```

Note: Only set image.repository and image.tag if the AIResource specifies a custom image. Otherwise use chart defaults.

The chart automatically:
1. Creates StatefulSet, ConfigMap, Services, PDB
2. Runs init Job to create the cluster (`CLUSTER MEET` + slot assignment)
3. All 16384 slots distributed across masters

## Scale Up (add masters)

To scale from 6 to 10 nodes (3→5 masters):

```
helm upgrade <release> oci://registry-1.docker.io/bitnamicharts/valkey-cluster \
  --namespace <ns> \
  --reuse-values \
  --set cluster.nodes=10 \
  --set cluster.update.addNodes=true \
  --set cluster.update.currentNumberOfNodes=6 \
  --set cluster.init=false \
  --wait --timeout 600s
```

The chart runs a post-upgrade Job that:
1. Adds new pods to the StatefulSet
2. Runs `CLUSTER MEET` to join new nodes
3. Rebalances slots across all masters

## Scale Down (remove masters)

To scale from 8 to 6 nodes (4→3 masters with 1 replica each):

**IMPORTANT**: The difference between old and new node count must be <= `cluster.replicas` to avoid removing a primary and all its replicas simultaneously. If scaling down by more, do it in multiple steps.

**Step 1**: Identify which nodes will be removed
The highest-numbered pods are removed first (e.g., pod-7, pod-6 when scaling from 8 to 6).
Run `CLUSTER NODES` to find their node IDs and check if they hold hash slots (masters).

```
kubectl exec <any-pod> -- valkey-cli -a $PASSWORD CLUSTER NODES
```

**Step 2**: Reshard slots BEFORE removing nodes
If any node being removed is a master holding slots, those slots MUST be migrated first.
Use `valkey-cli --cluster reshard` to move all slots from the departing master to remaining masters:

```
kubectl exec <any-pod> -- valkey-cli -a $PASSWORD --cluster reshard <any-node-ip>:6379 \
  --cluster-from <departing-node-id> \
  --cluster-to <target-node-id> \
  --cluster-slots <num-slots> \
  --cluster-yes
```

Repeat for each departing master until it holds 0 slots.
Verify with `CLUSTER NODES` — departing master should show no slot ranges.

**Step 3**: If the departing node is a master with replicas, those replicas must be reassigned:
```
kubectl exec <replica-pod> -- valkey-cli -a $PASSWORD CLUSTER REPLICATE <new-master-node-id>
```

**Step 4**: Helm upgrade to reduce node count
```
helm upgrade <release> oci://registry-1.docker.io/bitnamicharts/valkey-cluster \
  --namespace <ns> \
  --reuse-values \
  --set cluster.nodes=6 \
  --set cluster.init=false \
  --set password=<password> \
  --wait --timeout 600s
```

**Step 5**: After pods are removed, remaining nodes will show them as `fail`.
On EACH remaining pod, forget the failed nodes:
```
kubectl exec <each-remaining-pod> -- valkey-cli -a $PASSWORD CLUSTER FORGET <node-id>
```

**Step 6**: Verify cluster health
```
kubectl exec <any-pod> -- valkey-cli -a $PASSWORD CLUSTER INFO
```
Confirm `cluster_state:ok` and `cluster_slots_ok:16384`.

## Spec Change (memory, config)

```
helm upgrade <release> oci://registry-1.docker.io/bitnamicharts/valkey-cluster \
  --namespace <ns> \
  --reuse-values \
  --set valkey.resources.limits.memory=1Gi \
  --set valkey.resources.requests.memory=1Gi \
  --set cluster.init=false \
  --wait --timeout 600s
```

Pods will rolling-restart with new resource limits.

## Health Verification

- `kubectl exec <pod> -- valkey-cli -a $PASSWORD CLUSTER INFO` → check `cluster_state:ok`, `cluster_slots_ok:16384`
- `kubectl exec <pod> -- valkey-cli -a $PASSWORD CLUSTER NODES` → all nodes connected, no `fail` flags
- `helm status <release>` → status: deployed

## Healing

- **Pod crash**: K8s restarts automatically, Valkey auto-failover promotes replica
- **cluster_state:fail**: Check CLUSTER NODES for failed/disconnected nodes, may need CLUSTER FORGET + scale back up
- **NEVER re-initialize**: Do not use `cluster.init=true` on upgrade — it would destroy the existing cluster

## Constraints

- Minimum 6 nodes (3 masters × 2 with 1 replica each)
- Scale-down delta must be <= cluster.replicas per step
- Password cannot be rotated via Helm after initial deploy without deleting PVs
- Persistence must be enabled in production
