#!/usr/bin/env bash
# 60s ISUCON14 load against isucon14-1. Run from ubuntu-dev.
set -euo pipefail
TARGET="${TARGET:-192.168.0.210}"
BENCH="${BENCH:-192.168.0.213}"
ssh -o BatchMode=yes -o ServerAliveInterval=30 ubuntu@"$BENCH" \
  "sudo -u isucon /home/isucon/bench_linux_amd64 run \
    --target https://isuride.xiv.isucon.net \
    --addr ${TARGET}:443 \
    --payment-url http://${BENCH}:12345 \
    --payment-bind-port 12345 \
    -t 60 $*"
