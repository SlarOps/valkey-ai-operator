#!/bin/bash
# Args: $1 = replication_user (default: replicator), $2 = replication_password (default: replpass)
REP_USER="${1:-replicator}"
REP_PASS="${2:-replpass}"

# Create replication user
psql -U postgres -c "CREATE USER ${REP_USER} WITH REPLICATION ENCRYPTED PASSWORD '${REP_PASS}';" 2>/dev/null || true

# Update pg_hba.conf for replication
echo "host replication ${REP_USER} 0.0.0.0/0 md5" >> "$PGDATA/pg_hba.conf"

# Reload configuration
pg_ctl reload -D "$PGDATA"
echo "Primary initialized with replication user ${REP_USER}"
