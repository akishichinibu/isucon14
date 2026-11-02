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
  if [ -f "$ROOT/mysql/mysqld-isucon.cnf" ]; then
    scp -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
      "$ROOT/mysql/mysqld-isucon.cnf" "ubuntu@${ip}:/tmp/mysqld-isucon.cnf"
  fi
  scp -o BatchMode=yes -o StrictHostKeyChecking=accept-new \
    "$ROOT/nginx/isuride.conf" "ubuntu@${ip}:/tmp/isuride.conf"
  "${SSH[@]}" "ubuntu@${ip}" "sudo bash -s" << 'EOF'
set -euo pipefail
rsync -a --delete /tmp/isuride-go/ /home/isucon/webapp/go/
rsync -a --delete /tmp/isuride-sql/ /home/isucon/webapp/sql/
chown -R isucon:isucon /home/isucon/webapp/go /home/isucon/webapp/sql
if [ -f /tmp/mysqld-isucon.cnf ]; then
  install -m 0644 /tmp/mysqld-isucon.cnf /etc/mysql/mysql.conf.d/isucon.cnf
  systemctl restart mysql
fi
if [ -f /tmp/isuride.conf ]; then
  install -m 0644 /tmp/isuride.conf /etc/nginx/sites-available/isuride.conf
  nginx -t && systemctl reload nginx
fi
sudo -u isucon bash -lc 'cd /home/isucon/webapp/go && /home/isucon/local/golang/bin/go build -mod=vendor -o isuride -ldflags "-s -w"'
systemctl restart isuride-go
systemctl is-active isuride-go nginx mysql isuride-matcher
EOF
done
echo "deploy done"
