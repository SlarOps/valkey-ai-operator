# Valkey AI Operator

> **What if your Kubernetes operator could think?**

A K8s operator where the reconciliation logic is not code — it's an AI agent. Give it a CRD, and it figures out the rest: creates the cluster, scales it, heals it, patches resources — all by reasoning about desired vs actual state.

No giant `switch` statements. No 10,000-line controllers. Just: **"here's what you want, here's what exists — make it match."**

![Agent recovering a corrupted Valkey cluster](docs/images/agent-cluster-recovery.png)
*The agent autonomously recovers a corrupted Valkey cluster: diagnoses the failure, resets all 6 nodes with `CLUSTER RESET HARD`, reinitializes from scratch, verifies `cluster_state:ok`, and sets phase to Running — zero human intervention.*

## The Idea

Traditional K8s operators encode every possible scenario as code:

```
if no_statefulset → create
if replicas_mismatch → scale
if pod_crashed → restart
if memory_changed → patch
if cluster_state_fail → heal
if scaling_up → add_node + rebalance
... hundreds more conditions
```

This operator replaces all of that with:

```
1. Collect facts (desired vs actual)
2. Send to AI agent
3. Agent decides + executes
```

**17 lines of main.rs. Zero conditions. The agent handles everything.**

## How It Works

```
K8s API Server
      │
      │  watches ValkeyCluster CRD
      ▼
┌─────────────────────┐
│  Reconciler          │
│                      │
│  Every change:       │         ┌──────────────────────────┐
│  1. Get CRD spec     │         │  AI Agent (Claude)       │
│  2. Get StatefulSet   │  ───►  │                          │
│  3. List Pods         │  State │  Receives:               │
│  4. Exec CLUSTER INFO │  Snap  │  - Desired vs Actual     │
│  5. Detect diff       │  shot  │  - What changed          │
│                      │         │                          │
│  NO decisions here   │         │  Decides autonomously:   │
│  NO phase logic      │         │  - Create? Scale? Heal?  │
│  Just facts.         │         │  - Execute tools         │
└─────────────────────┘         │  - Update CRD status     │
                                └──────────────────────────┘
```

**The reconciler never decides what to do.** It builds a `StateSnapshot` and sends it to the agent. The agent is the sole decision maker.

### What the agent sees

```
## Cluster 'my-valkey' — Trigger: memory_mismatch: sts=512Mi spec=1Gi

### Desired: masters=3, replicas=1, memory=1Gi
### Actual:  sts=6 pods, memory=512Mi, cluster_state=ok, slots=16384/16384

### What Needs Attention
- Memory limit mismatch: sts=512Mi, spec=1Gi
```

Agent reads this → calls `patch_resources` → verifies health → updates status. Done.

## It Actually Works

Tested on a real kind cluster with Vertex AI (Claude Haiku):

| Scenario | Result |
|----------|--------|
| `kubectl apply` ValkeyCluster | Agent creates ConfigMap, Services, StatefulSet, runs `valkey-cli --cluster create` |
| `kubectl delete pod my-valkey-1` | Valkey auto-failover, replica promotes to master, cluster stays `ok` |
| `kubectl patch` masters 3 → 4 | Agent scales StatefulSet, adds nodes, rebalances slots |
| `kubectl patch` memory 512Mi → 1Gi | Agent detects mismatch, JSON-patches StatefulSet |
| Cluster state = `fail` | Agent runs `CLUSTER RESET HARD` on all pods, reinitializes from scratch |
| Nothing changed | Agent skipped entirely (saves API cost) |

## Quick Start

```bash
# Prerequisites: Rust 1.75+, a K8s cluster, gcloud auth application-default login

# Install CRD
cargo run --bin gen_crd > manifests/crd.yaml
kubectl apply -f manifests/crd.yaml

# Run operator
RUST_LOG=valkey_ai_operator=info cargo run

# Create a Valkey cluster
kubectl apply -f manifests/samples/valkeycluster.yaml

# Watch the agent work
kubectl get valkeycluster -w
```

### Try Breaking Things

```bash
# Scale
kubectl patch valkeycluster my-valkey --type merge -p '{"spec":{"masters":4}}'

# Change resources
kubectl patch valkeycluster my-valkey --type merge -p '{"spec":{"resources":{"limits":{"memory":"256Mi"}}}}'

# Kill a master
kubectl delete pod my-valkey-1

# Watch the agent reason, diagnose, and fix — in real time
```

## Sample CRD

```yaml
apiVersion: valkey.krust.io/v1alpha1
kind: ValkeyCluster
metadata:
  name: my-valkey
spec:
  version: "7"
  masters: 3
  replicas_per_master: 1
  resources:
    requests:
      memory: "128Mi"
      cpu: "100m"
    limits:
      memory: "256Mi"
      cpu: "250m"
  agent:
    enabled: true
    self_healing: true
    provider: vertex           # or "anthropic"
    region: us-east5
    project_id: your-gcp-project-id
```

## Agent Tools (18)

The agent has 18 tools — it picks whichever ones it needs:

| Category | Tools |
|----------|-------|
| **Create** | `create_configmap`, `create_service`, `create_statefulset` |
| **Scale** | `scale_statefulset`, `cluster_add_node`, `cluster_rebalance` |
| **Heal** | `restart_pod`, `cluster_init`, `valkey_cli` (CLUSTER RESET, MEET, etc.) |
| **Observe** | `cluster_info`, `cluster_nodes`, `health_check`, `get_pod_status`, `get_pod_logs`, `get_events`, `wait_for_pods`, `pod_exec` |
| **Update** | `patch_resources`, `update_cluster_status` |

## Safety

The agent is powerful but constrained:

- **Guardrails** — memory scaling capped at `maxMemoryScaleFactor`, cannot delete StatefulSet
- **Circuit breaker** — 3 consecutive failures → `Failed` phase, stops retrying
- **Command denylist** — FLUSHALL, FLUSHDB, DEBUG, SHUTDOWN blocked
- **No-op detection** — agent only called when state actually changed
- **Audit trail** — every action logged to CRD status + K8s Events

## Project Structure

```
src/
├── main.rs              # 17 lines. Seriously.
├── controller/mod.rs    # Reconciler: facts only, no decisions
├── types.rs             # StateSnapshot — the bridge between K8s and AI
├── agent/
│   ├── worker.rs        # One system prompt. Receive snapshot. Run agent.
│   ├── agent.rs         # Autonomous tool-calling loop
│   └── provider.rs      # Vertex AI + Anthropic with exponential backoff
├── tools/
│   ├── k8s.rs           # 12 K8s tools with guardrails
│   └── valkey.rs        # 7 Valkey tools (exec-based, works outside cluster)
└── crd.rs               # ValkeyCluster CRD
```

## Why This Matters

This isn't about Valkey. It's about the pattern:

1. **Any CRD** can use this architecture — PostgreSQL, Kafka, Elasticsearch
2. **Zero domain logic in the controller** — the AI brings the domain knowledge
3. **Self-healing by reasoning**, not by matching known failure patterns
4. **New failure modes don't need new code** — the agent figures them out

The operator that ships with 0 `if` statements for operations — because the AI already knows how databases work.

## Built With

- [kube-rs](https://github.com/kube-rs/kube) — Kubernetes controller runtime (Rust)
- [Claude](https://docs.anthropic.com/en/docs/about-claude/models) via Vertex AI — the brain
- Agent engine from [krust](https://github.com/vanchonlee/krust) (ZeroClaw pattern)

## Status

**Proof of concept.** It works. It's not production-ready. But it proves the idea: AI-driven operators are not just possible — they're simpler than the alternative.

## License

MIT
