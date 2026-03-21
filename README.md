# Krust Operator

> **What if your Kubernetes operator could think?**

A generic AI-driven K8s operator where reconciliation logic is not code — it's an AI agent. Define a **skill** (what to manage), create an **AIResource** (what you want), and the agent figures out the rest: deploys, scales, heals, patches — all by reasoning about desired vs actual state.

No giant `switch` statements. No 10,000-line controllers. Just markdown skills and natural language goals.

## Why Not Just Helm?

Different tools, different jobs.

**Helm** is a package manager — it renders templates, applies them, done. For Day 1 deployment, Helm (or Helm + ArgoCD/Flux for GitOps drift detection) is battle-tested and deterministic.

**Krust** explores a different idea: what if the **reconciliation logic itself** was written in natural language instead of code?

| | Helm / GitOps | Traditional Operator (Go/Rust) | Krust |
|---|------|---|---|
| **Deploy** | Templates + values | Hardcoded in controller | Agent reads skill, decides |
| **Drift healing** | ArgoCD re-syncs manifests | Coded reconcile loop | Agent re-applies from desired state |
| **Health response** | External alerting needed | Coded per-workload | Agent reasons from skill knowledge |
| **New workload** | New chart (~same effort) | Thousands of lines of code | New skill directory (markdown + templates) |
| **Behavior is...** | Deterministic | Deterministic | Non-deterministic (LLM) |

The real comparison is **Krust vs traditional operators** (like the Redis operator, PostgreSQL operator, etc.). Those require thousands of lines of Go/Rust per workload. Krust replaces that code with markdown skills that an LLM agent interprets at runtime.

### Trade-offs

Krust trades **determinism for flexibility**:

- **Pro**: Adding PostgreSQL support = 0 lines of Rust, just markdown + YAML + shell
- **Pro**: Agent can reason about novel situations not explicitly coded
- **Con**: Each reconciliation costs an LLM API call (~30s latency, ~$0.01)
- **Con**: Agent can make mistakes — the simulator catches some, but not all
- **Con**: Helm/ArgoCD are production-proven; Krust is a proof of concept

**Use Helm** for straightforward deployments. **Use a traditional operator** when you need battle-tested, deterministic Day 2 operations. **Explore Krust** if you're interested in whether LLM agents can replace hand-written operator code.

## Architecture

```
                    ┌──────────────┐
                    │  AIResource  │  "Deploy PostgreSQL with 1Gi memory"
                    │  (goal+skill)│
                    └──────┬───────┘
                           │
              ┌────────────▼────────────┐
              │      Controller         │
              │  watches + detects:     │
              │  • bootstrap            │
              │  • spec change          │
              │  • drift (missing child)│
              └────────────┬────────────┘
                           │ events
              ┌────────────▼────────────┐
              │    Multi-Agent Pipeline  │
              │                         │
              │  High risk (bootstrap): │
              │  Planner → Simulator    │
              │  → Executor → Verifier  │
              │                         │
              │  Low risk (drift):      │
              │  Executor → Verifier    │
              └────────────┬────────────┘
                           │ tools
              ┌────────────▼────────────┐
              │    K8s Runtime          │
              │  apply_template         │
              │  run_action             │
              │  get_state              │
              │  wait_for_ready         │
              └─────────────────────────┘
```

### Design Principles

- **Skill** = source of truth — markdown defines what the agent knows and can do
- **Agent** = decision maker — LLM reasons about state and decides actions
- **K8s runtime** = delegated executor — the agent's tools, not the controller's logic

Adding a new workload (PostgreSQL, Kafka, etc.) requires **zero Rust code** — just a new skill directory with markdown, YAML templates, and shell scripts.

## Quick Start

```bash
# Prerequisites: Rust 1.75+, a K8s cluster (OrbStack, kind, etc.)

# Install CRD and RBAC
kubectl apply -f manifests/crd.yaml
kubectl apply -f manifests/rbac.yaml

# Run operator locally
SKILLS_DIR=./skills RUST_LOG=info cargo run --bin krust-operator

# Deploy a PostgreSQL instance
kubectl apply -f manifests/samples/postgresql.yaml

# Watch the agent work
kubectl get airesource -w
```

## Examples

### PostgreSQL

```yaml
apiVersion: krust.io/v1
kind: AIResource
metadata:
  name: my-postgres
spec:
  skill: postgresql
  goal: "Deploy a single PostgreSQL instance with 1Gi memory"
  image: postgres:16
  resources:
    limits:
      memory: "1Gi"
      cpu: "500m"
  agent:
    provider: vertex
    region: us-east5
    model: claude-haiku-4-5@20251001
```

The agent will: create ConfigMap (postgresql.conf + pg_hba.conf), create Service (port 5432), create StatefulSet with 1 pod, wait for ready, run `pg_isready` health check, verify config, set status to Running.

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
    region: us-east5
```

### 3-Master Valkey Cluster

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
    region: us-east5
    model: claude-haiku-4-5@20251001
  guardrails:
    max_replicas: 12
    max_memory: "4Gi"
    denied_commands: ["FLUSHALL", "FLUSHDB", "DEBUG", "SHUTDOWN"]
```

The agent will: calculate 6 pods (3 masters + 3 replicas), apply ConfigMap + Service + StatefulSet, wait for all pods, get pod IPs, run `cluster_init`, verify `cluster_state=ok` with 16384 slots assigned.

### Self-Healing

```bash
# Delete a child resource — agent detects drift and re-applies
kubectl delete configmap my-postgres-config

# Change the goal — agent re-plans and executes
kubectl patch airesource my-postgres --type merge \
  -p '{"spec":{"goal":"Deploy a single PostgreSQL instance with 2Gi memory"}}'

# Delete the AIResource — K8s garbage collects all child resources
kubectl delete airesource my-postgres
```

## Skills

Skills define **what** the operator can manage. Each skill is a self-contained directory:

```
skills/
├── postgresql/
│   ├── SKILL.md              # Domain knowledge + frontmatter (monitors, actions, agents)
│   ├── prompts/
│   │   ├── planner.md        # "Create a deployment plan as JSON..."
│   │   ├── simulator.md      # "Validate plan for safety..."
│   │   ├── executor.md       # "Execute using these tools..."
│   │   └── verifier.md       # "Verify state matches goal..."
│   ├── scripts/
│   │   ├── get_config.sh
│   │   └── monitors/
│   │       └── health_check.sh
│   └── templates/
│       ├── configmap.yaml
│       ├── service.yaml
│       └── statefulset.yaml
│
└── valkey-cluster/
    ├── SKILL.md
    ├── prompts/
    ├── scripts/
    │   ├── cluster_init.sh
    │   ├── add_node.sh
    │   ├── rebalance.sh
    │   ├── get_config.sh
    │   └── monitors/
    └── templates/
```

### SKILL.md Format

```yaml
---
name: postgresql
description: Manage PostgreSQL on Kubernetes. Handles deployment, configuration, health monitoring, and self-healing.
allowed-tools: run_action, apply_template, get_state, update_status, get_pod_logs, wait_for_ready, get_events

monitors:
  - name: pg_health
    interval: 15s
    script: scripts/monitors/health_check.sh
    parse: exit-code
    trigger_when: "exit_code != 0"

actions:
  - name: health_check
    risk: low
    description: Check PostgreSQL is accepting connections
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

# PostgreSQL Knowledge

(Markdown body: deployment guide, configuration rules, drift healing procedures, guardrails...)
```

The agent reads the markdown body as domain knowledge and uses the declared actions and templates as tools. The SKILL.md is the single source of truth for how to manage the workload.

## Agent Pipeline

| Risk Level | Trigger | Pipeline | Duration |
|------------|---------|----------|----------|
| **High** | Bootstrap (first deploy) | Planner → Simulator → Executor → Verifier | ~30s |
| **Medium** | Spec change | Planner → Simulator → Executor → Verifier | ~30s |
| **Low** | Drift (missing child resource) | Executor → Verifier | ~15s |

- **Planner** — reads skill knowledge + current state, outputs JSON action plan
- **Simulator** — validates plan safety (rejected plans get re-planned, up to 3 attempts)
- **Executor** — runs the plan using tools (apply_template, run_action, wait_for_ready)
- **Verifier** — confirms actual state matches goal, updates AIResource status

## Drift Healing

The controller sets `ownerReferences` on all child resources. When a child is deleted:

1. K8s `.owns()` watch triggers reconcile on the parent AIResource
2. Controller reads `krust.io/desired-state` annotation (stored rendered manifests)
3. Controller checks each resource exists — if missing, sends `DriftDetected` event
4. Agent receives event as **Low risk** — executor re-applies only the missing resource
5. Verifier confirms health

```
kubectl delete configmap my-postgres-config
  → .owns() triggers reconcile
  → detect_drift() finds ConfigMap missing
  → DriftDetected event → agent
  → executor: apply_template(configmap.yaml)
  → verifier: health_check → status=Running
```

When the AIResource itself is deleted, K8s garbage collection automatically cleans up all child resources.

## Agent Tools

| Tool | Description |
|------|-------------|
| `apply_template` | Render and apply a K8s manifest template with ownerReference injection |
| `run_action` | Execute a skill-defined script in a pod |
| `get_state` | Get current K8s state (pods, statefulsets, monitors) |
| `update_status` | Update AIResource status (phase, message) |
| `get_pod_logs` | Get pod logs (last 100 lines) |
| `wait_for_ready` | Poll until expected pods are ready |
| `get_events` | List K8s events for a resource |

## Safety

- **Multi-agent pipeline** — separate agents for planning, validation, execution, verification
- **Simulator** — rejects unsafe plans (e.g., cluster_init on existing cluster with data)
- **Circuit breaker** — 3 consecutive failures → stops retrying, enters Failed phase
- **Guardrails** — per-resource: max replicas, max memory, denied commands
- **Risk-based routing** — low-risk events skip planner/simulator for faster healing
- **ownerReferences** — automatic garbage collection on AIResource deletion

## Configuration

### AIResource Spec

| Field | Description | Required |
|-------|-------------|----------|
| `spec.skill` | Skill name (directory under SKILLS_DIR) | Yes |
| `spec.goal` | Natural language goal for the agent | Yes |
| `spec.image` | Container image to deploy | Yes |
| `spec.resources` | Container resource limits | No |
| `spec.agent.provider` | `anthropic` or `vertex` | No (default: anthropic) |
| `spec.agent.model` | Model ID | No |
| `spec.agent.project_id` | GCP project for Vertex AI | If provider=vertex |
| `spec.agent.region` | GCP region | If provider=vertex |
| `spec.guardrails` | Safety constraints | No |

### Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `SKILLS_DIR` | Path to skills directory | `/skills` |
| `RUST_LOG` | Log level | `info` |
| `ANTHROPIC_API_KEY` | API key (if provider=anthropic) | — |
| `ANTHROPIC_DEFAULT_MODEL` | Override default model | — |
| `CLOUD_ML_REGION` | Override Vertex AI region | — |

## Project Structure

```
src/
├── main.rs              # Entry point
├── controller/
│   ├── mod.rs           # Reconciler: watches, drift detection, event routing
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
│   ├── desired_state.rs # Desired-state annotation read/write + ownerRef helper
│   ├── k8s.rs           # K8s tools (server-side apply, pod status, logs, events)
│   ├── runtime.rs       # RunAction + ApplyTemplate (with ownerRef + desired-state storage)
│   ├── state.rs         # GetState + UpdateStatus
│   ├── template.rs      # Template variable rendering
│   └── mod.rs           # Tool registration per pipeline role
├── monitor/             # Monitor registry + runner
├── channel.rs           # Per-resource event channels
├── crd.rs               # AIResource CRD definition
└── types.rs             # StateSnapshot, ResourceEvent, DriftInfo, CircuitBreaker
```

## Built With

- [kube-rs](https://github.com/kube-rs/kube) — Kubernetes controller runtime (Rust)
- [Claude](https://docs.anthropic.com/en/docs/about-claude/models) via Vertex AI or Anthropic API

## License

MIT
