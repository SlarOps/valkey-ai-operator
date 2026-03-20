#!/bin/bash
valkey-cli -p ${VALKEY_PORT:-6379} PING 2>/dev/null | grep -q PONG
