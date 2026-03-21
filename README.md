# Krust Operator

> **What if your Kubernetes operator could think?**

A generic AI-driven K8s operator where reconciliation logic is not code — it's an AI agent. Define a **skill** (what to manage), create an **AIResource** (what you want), and the agent figures out the rest: deploys, scales, heals, patches — all by reasoning about desired vs actual state.

No giant `switch` statements. No 10,000-line controllers. Just: **"here's what you want, here's what exists — make it match."**

## How It Works

```
┌─────────────────┐     ┌──────────────────────────────────────────┐
│  K8s API Server  │     │  Multi-Agent Pipeline                    │
│                  │     │                                          │
│  AIResource CR   │────►│  Planner ─► Simulator ─► Executor ─► Verifier │
│  (goal + skill)  │     │                                          │
│                  │◄────│  Tools: apply_template, run_action,      │
│  StatefulSet,    │     │         get_state, update_status, ...    │
│  Pods, Services  │     │                                          │
└─────────────────┘     └──────────────────────────────────────────┘
```

1. **Controller** watches AIResource CRDs, collects facts (desired vs actual state)
2. **Planner** agent creates an action plan based on goal + current state
3. **Simulator** validates the plan for safety (high-risk operations only)
4. **Executor** agent runs the plan using skill-defined tools
5. **Verifier** agent confirms the result matches the goal

## Quick Start

```bash
# Prerequisites: Rust 1.75+, a K8s cluster (OrbStack, kind, etc.)

# Install CRD
kubectl apply -f manifests/crd.yaml

# Setup RBAC
kubectl apply -f manifests/rbac.yaml

# Run operator locally
RUST_LOG=info,krust_operator=debug SKILLS_DIR=./skills cargo run --bin krust-operator

# Create a Valkey instance
kubectl apply -f manifests/samples/valkey-cluster.yaml

# Watch the agent work
kubectl get airesource -w
```

## Examples

### Single Valkey Instance

```yaml
apiVersion: krust.io/v1
kind: AIResource
metadata:
  name: my-valkey
spec:
  skill: valkey-cluster
  goal: "Run a Valkey instance, 1Gi memory"
  image: valkey/valkey:7
  agent:
    provider: vertex
    project_id: your-gcp-project
    region: us-east5
```

The agent will: create ConfigMap (standalone mode, no cluster), create Service, create StatefulSet with 1 pod, verify health, set status to Running.

### 3-Master Valkey Cluster with Replicas

```yaml
apiVersion: krust.io/v1
kind: AIResource
metadata:
  name: my-cluster
spec:
  skill: valkey-cluster
  goal: "Run a 3-master Valkey cluster with 1 replica each, 2Gi memory"
  image: valkey/valkey:7
  resources:
    limits:
      memory: "2Gi"
      cpu: "500m"
  agent:
    provider: vertex
    project_id: your-gcp-project
    region: us-east5
    model: claude-haiku-4-5@20251001
  guardrails:
    max_replicas: 12
    max_memory: "4Gi"
    denied_commands: ["FLUSHALL", "FLUSHDB", "DEBUG", "SHUTDOWN"]
```

The agent will: calculate 6 pods needed, apply ConfigMap + Service + StatefulSet, wait for all pods ready, get pod IPs, run `cluster_init` with all 6 IPs, verify `cluster_state=ok` with 16384 slots assigned.

### Try Breaking Things

```bash
# Scale up: change goal from 3-master to 4-master
kubectl patch airesource my-cluster --type merge \
  -p '{"spec":{"goal":"Run a 4-master Valkey cluster with 1 replica each, 2Gi memory"}}'

# Change memory
kubectl patch airesource my-cluster --type merge \
  -p '{"spec":{"goal":"Run a 3-master Valkey cluster with 1 replica each, 4Gi memory"}}'

# Delete configmap — agent will recreate it
kubectl delete configmap my-cluster-config

# Kill a pod — Valkey auto-failover + agent verifies health
kubectl delete pod my-cluster-1
```

## Skills

Skills define **what** the operator can manage. Each skill is a directory with:

```
skills/valkey-cluster/
├── SKILL.md              # Knowledge, actions, monitors, agent prompts
├── scripts/
│   ├── cluster_init.sh   # Initialize Valkey cluster
│   ├── add_node.sh       # Add node to cluster
│   ├── rebalance.sh      # Rebalance slots
│   ├── get_config.sh     # Get runtime config
│   └── monitors/
│       ├── cluster_info.sh
│       └── health_check.sh
└── templates/
    ├── statefulset.yaml   # K8s manifest templates
    ├── service.yaml
    └── configmap.yaml
```

The agent reads `SKILL.md` for domain knowledge (how to deploy, scale, heal) and uses the defined actions and templates as tools. Adding a new skill (e.g., PostgreSQL, Kafka) requires no code changes — just a new skill directory.

## Agent Tools

The agent gets tools based on the skill definition:

| Tool | Description |
|------|-------------|
| `apply_template` | Render and apply a K8s manifest template |
| `run_action` | Execute a skill-defined script in a pod |
| `get_state` | Get current K8s state (pods, statefulsets, monitors) |
| `update_status` | Update AIResource status (phase, message) |
| `get_pod_logs` | Get pod logs (last 100 lines) |
| `wait_for_ready` | Poll until expected pods are ready |
| `get_events` | List K8s events for a resource |

## Safety

- **Multi-agent pipeline** — Planner creates plan, Simulator validates safety, Executor runs, Verifier confirms
- **Simulator retry** — rejected plans get re-planned with simulator feedback (up to 3 attempts)
- **Circuit breaker** — 3 consecutive failures → stops retrying, enters Failed phase
- **Guardrails** — max replicas, max memory, denied commands (configurable per AIResource)
- **Risk levels** — Low (executor only), Medium (planner + executor), High (full pipeline with simulator)
- **Spec change detection** — controller detects CR changes and sends events to running agents

## Project Structure

```
src/
├── main.rs              # Entry point
├── controller/
│   ├── mod.rs           # Reconciler: facts only, no decisions
│   └── status.rs        # CRD status updates
├── agent/
│   ├── agent.rs         # Autonomous tool-calling loop
│   ├── worker.rs        # Agent instance lifecycle + circuit breaker
│   ├── provider.rs      # Vertex AI + Anthropic API
│   ├── tool.rs          # Tool trait + safety levels
│   └── types.rs         # Agent types
├── pipeline/
│   ├── mod.rs           # Multi-agent pipeline orchestration
│   ├── planner.rs       # Plan generation agent
│   ├── simulator.rs     # Plan validation agent
│   ├── executor.rs      # Plan execution agent
│   └── verifier.rs      # Result verification agent
├── skill/
│   ├── loader.rs        # SKILL.md parser (YAML frontmatter + markdown)
│   ├── types.rs         # Skill config types
│   └── trigger.rs       # Monitor trigger evaluation
├── tools/
│   ├── k8s.rs           # K8s tools (pod status, logs, events, wait)
│   ├── runtime.rs       # RunAction + ApplyTemplate
│   ├── state.rs         # GetState + UpdateStatus
│   └── template.rs      # Template variable rendering
├── monitor/             # Monitor registry + runner
├── channel.rs           # Per-resource event channels
├── crd.rs               # AIResource CRD definition
└── types.rs             # StateSnapshot, ResourceEvent, CircuitBreaker
```

## Configuration

### AIResource Spec

| Field | Description | Required |
|-------|-------------|----------|
| `spec.skill` | Skill name (directory under SKILLS_DIR) | Yes |
| `spec.goal` | Natural language goal for the agent | Yes |
| `spec.image` | Container image to deploy | Yes |
| `spec.agent.provider` | `anthropic` or `vertex` | No (default: anthropic) |
| `spec.agent.model` | Model ID | No (default: claude-haiku-4-5-20251001) |
| `spec.agent.project_id` | GCP project for Vertex AI | If provider=vertex |
| `spec.agent.region` | GCP region | If provider=vertex |
| `spec.guardrails` | Safety constraints | No |

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `SKILLS_DIR` | Path to skills directory | `/skills` |
| `RUST_LOG` | Log level | `info` |
| `ANTHROPIC_API_KEY` | API key (if provider=anthropic) | — |

## Built With

- [kube-rs](https://github.com/kube-rs/kube) — Kubernetes controller runtime (Rust)
- [Claude](https://docs.anthropic.com/en/docs/about-claude/models) via Vertex AI or Anthropic API

## License

MIT
