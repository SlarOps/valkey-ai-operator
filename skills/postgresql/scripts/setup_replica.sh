#!/bin/bash
# Env vars: PRIMARY_HOST (required), REPLICATION_USER (default: replicator), REPLICATION_PASSWORD (default: replpass)
PRIMARY_HOST="${PRIMARY_HOST:?PRIMARY_HOST is required}"
REP_USER="${REPLICATION_USER:-replicator}"
REP_PASS="${REPLICATION_PASSWORD:-replpass}"

# Stop PostgreSQL if running
pg_ctl stop -D "$PGDATA" -m fast 2>/dev/null || true

# Clean data directory
rm -rf "$PGDATA"/*

# Run pg_basebackup from primary
PGPASSWORD="$REP_PASS" pg_basebackup -h "$PRIMARY_HOST" -U "$REP_USER" -D "$PGDATA" -Fp -Xs -P -R

# Create standby signal
touch "$PGDATA/standby.signal"

# Start PostgreSQL
pg_ctl start -D "$PGDATA"
echo "Replica setup complete, streaming from ${PRIMARY_HOST}"
