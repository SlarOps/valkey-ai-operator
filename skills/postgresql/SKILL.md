---
name: postgresql
description: Manage PostgreSQL on Kubernetes. Handles single instance deployment, configuration, health monitoring, and self-healing.
allowed-tools: run_action, apply_template, get_state, update_status, get_pod_logs, wait_for_ready, get_events, kubectl_describe, kubectl_get, kubectl_scale, kubectl_patch, kubectl_exec

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
  - name: get_config
    risk: low
    description: Get PostgreSQL runtime configuration
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

# PostgreSQL Knowledge

PostgreSQL is a powerful open-source relational database.

## Deployment Mode

This skill manages a **single PostgreSQL instance** with persistent storage.

## Deployment Guide

### Step 1: Determine Configuration
From the goal, extract:
- `memory_limit`: memory for container (e.g., "1Gi", "2Gi")
- `cpu_limit`: CPU for container (e.g., "500m", "1000m")
- `max_connections`: default 100
- `shared_buffers`: typically 25% of memory. Use PostgreSQL format: "256MB", "512MB", "1GB"
  - 1Gi memory → shared_buffers = "256MB"
  - 2Gi memory → shared_buffers = "512MB"
  - 4Gi memory → shared_buffers = "1GB"
- `work_mem`: typically 4MB for general use
- `storage`: disk size, default "10Gi"

### Step 2: Apply Kubernetes Resources (in this order)
1. **configmap.yaml** — PostgreSQL configuration
   - vars: `name`, `namespace`, `max_connections`, `shared_buffers`, `work_mem`
2. **service.yaml** — ClusterIP service for client connections
   - vars: `name`, `namespace`, `port` (default: 5432)
3. **statefulset.yaml** — the PostgreSQL pod
   - vars: `name`, `namespace`, `image`, `replicas` (always 1), `memory_limit`, `cpu_limit`, `storage`

### Step 3: Wait for Pod Ready
- Use `wait_for_ready` with `expected_count` = 1, `timeout_seconds` = 120
- Pod must be Running and Ready before proceeding

### Step 4: Verify
- Run `health_check` to verify PostgreSQL accepts connections
- Run `get_config` to verify shared_buffers and max_connections
- Update status to Running if healthy

## Drift Healing
When the trigger source is "drift" and reason mentions missing resources:
1. Re-apply the missing templates using `apply_template` with the same variables as original deployment
2. Use `get_state` to check current state and determine what variables to use
3. After re-applying, verify PostgreSQL is healthy
4. Update status accordingly

## Spec Change Handling
When spec changes (e.g., memory increase):
1. Update configmap with new shared_buffers
2. Re-apply statefulset with new resource limits
3. Pod will rolling-restart with new config
4. Verify health after restart

## Health Check
- `pg_isready` → checks if PostgreSQL accepts connections

## Guardrails
- NEVER drop databases without explicit confirmation
- max_connections should not exceed 500 for single instance
- shared_buffers should not exceed 40% of available memory

## Port
- Default PostgreSQL port: 5432
