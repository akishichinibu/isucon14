#!/usr/bin/env bash
# Deploy webapp/go + sql to the three app VMs and restart isuride-go.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
HOSTS=(192.168.0.210 192.168.0.211 192.168.0.212)
SSH=(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new)
RSYNC=(rsync -az --delete -e "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new")

for ip in "${HOSTS[@]}"; do
  echo "==== deploy $ip ===="
  "${RSYNC[@]}" "$ROOT/webapp/go/" "ubuntu@${ip}:/tmp/isuride-go/"
  "${RSYNC[@]}" "$ROOT/webapp/sql/" "ubuntu@${ip}:/tmp/isuride-sql/"
  "${SSH[@]}" "ubuntu@${ip}" "sudo bash -s" << 'EOF'
set -euo pipefail
rsync -a --delete /tmp/isuride-go/ /home/isucon/webapp/go/
rsync -a --delete /tmp/isuride-sql/ /home/isucon/webapp/sql/
chown -R isucon:isucon /home/isucon/webapp/go /home/isucon/webapp/sql
sudo -u isucon bash -lc 'cd /home/isucon/webapp/go && /home/isucon/local/golang/bin/go build -mod=vendor -o isuride -ldflags "-s -w"'
systemctl restart isuride-go
systemctl is-active isuride-go nginx mysql isuride-matcher
EOF
done
echo "deploy done"
