#!/bin/bash
psql -U postgres -t -A -c "SELECT name, setting FROM pg_settings WHERE name IN ('max_connections', 'shared_buffers', 'work_mem', 'effective_cache_size');"
