#!/bin/bash
# Env vars: REPLICATION_USER (default: replicator), REPLICATION_PASSWORD (default: replpass)
REP_USER="${REPLICATION_USER:-replicator}"
REP_PASS="${REPLICATION_PASSWORD:-replpass}"

# Create replication user
psql -U postgres -c "CREATE USER ${REP_USER} WITH REPLICATION ENCRYPTED PASSWORD '${REP_PASS}';" 2>/dev/null || true

# Update pg_hba.conf for replication
echo "host replication ${REP_USER} 0.0.0.0/0 md5" >> "$PGDATA/pg_hba.conf"

# Reload configuration
pg_ctl reload -D "$PGDATA"
echo "Primary initialized with replication user ${REP_USER}"
