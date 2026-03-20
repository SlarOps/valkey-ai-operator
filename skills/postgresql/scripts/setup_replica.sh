#!/bin/bash
# Args: $1 = primary_host, $2 = replication_user (default: replicator), $3 = replication_password (default: replpass)
PRIMARY_HOST="$1"
REP_USER="${2:-replicator}"
REP_PASS="${3:-replpass}"

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
