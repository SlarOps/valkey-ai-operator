---
name: postgresql
description: Manage PostgreSQL primary-replica setup on Kubernetes. Handles creation, replica setup, failover, and health monitoring.
allowed-tools: run_action, apply_template, get_state, update_status, get_pod_logs, wait_for_ready, get_events

monitors:
  - name: pg_ready
    interval: 10s
    script: scripts/monitors/pg_isready.sh
    parse: exit-code
    trigger_when: "exit_code != 0"
  - name: replication
    interval: 30s
    script: scripts/monitors/replication_check.sh
    parse: key-value
    trigger_when: "replica_count < 1"

actions:
  - name: init_primary
    risk: high
    description: Initialize PostgreSQL primary with replication configuration
    script: scripts/init_primary.sh
    params: [replication_user, replication_password]
  - name: setup_replica
    risk: medium
    description: Set up a new replica from primary using pg_basebackup
    script: scripts/setup_replica.sh
    params: [primary_host, replication_user, replication_password]
  - name: failover
    risk: high
    description: Promote a replica to primary
    script: scripts/failover.sh
    params: [target_pod]
  - name: health_check
    risk: low
    description: Check PostgreSQL health
    script: scripts/monitors/pg_isready.sh

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

# PostgreSQL Primary-Replica Knowledge

PostgreSQL uses streaming replication for high availability. One primary accepts writes; replicas stream WAL (Write-Ahead Log) from the primary for read scaling and failover.

## Creating a New Setup
1. Apply StatefulSet template for primary (replicas=1 initially)
2. Apply Services (primary service + headless service)
3. Wait for primary pod to be ready
4. Run init_primary action to configure replication user and pg_hba.conf
5. Scale StatefulSet to include replicas
6. Wait for replica pods to be ready
7. Run setup_replica for each replica pod

## Pod Roles
- Pod-0 is always the initial primary
- Pod-1, Pod-2, etc. are replicas
- After failover, roles may change

## Scaling Replicas
1. Increase StatefulSet replicas
2. Wait for new pods to be ready
3. Run setup_replica for each new pod

## Failover
1. Identify a healthy replica
2. Run failover action to promote it (pg_ctl promote)
3. Reconfigure other replicas to follow new primary
4. Update services to point to new primary

## Health Checks
- pg_isready: checks if PostgreSQL is accepting connections
- Replication check: queries pg_stat_replication on primary for lag and replica count

## Important Ports
- Default port: 5432

## Configuration
- wal_level = replica
- max_wal_senders = 10
- hot_standby = on
- Primary identified by absence of standby.signal file
