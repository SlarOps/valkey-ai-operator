#!/bin/bash
# Promote this replica to primary
pg_ctl promote -D "$PGDATA"
echo "Failover complete — this node is now primary"
