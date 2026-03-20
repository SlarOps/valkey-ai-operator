#!/bin/bash
# Run on primary to check replication status
RESULT=$(psql -U postgres -t -c "SELECT count(*) FROM pg_stat_replication" 2>/dev/null)
REPLICA_COUNT=$(echo "$RESULT" | tr -d ' ')
LAG=$(psql -U postgres -t -c "SELECT COALESCE(MAX(EXTRACT(EPOCH FROM replay_lag)), 0)::int FROM pg_stat_replication" 2>/dev/null | tr -d ' ')
echo "replica_count=${REPLICA_COUNT:-0}"
echo "replication_lag=${LAG:-0}"
