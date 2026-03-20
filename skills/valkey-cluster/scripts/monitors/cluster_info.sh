#!/bin/bash
valkey-cli -p ${VALKEY_PORT:-6379} CLUSTER INFO 2>/dev/null | grep -E '^(cluster_state|cluster_slots_ok|cluster_known_nodes|cluster_size)' | tr ':' '='
