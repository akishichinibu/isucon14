#!/usr/bin/env bash
# .210 nginx LB + stateless rust
# .211 stateful rust (in-memory matching/coords)
# .212 MySQL only
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN="$ROOT/webapp/rust/target/release/isuride"
SSH=(ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new)
RSYNC=(rsync -az --delete -e "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new")

deploy_app() {
  local ip="$1" matching="$2"
  echo "==== app $ip matching=$matching ===="
  "${RSYNC[@]}" --exclude target "$ROOT/webapp/rust/" "ubuntu@${ip}:/tmp/isuride-rust/"
  "${RSYNC[@]}" "$ROOT/webapp/sql/" "ubuntu@${ip}:/tmp/isuride-sql/"
  scp -o BatchMode=yes "$BIN" "ubuntu@${ip}:/tmp/isuride-rust-bin"
  scp -o BatchMode=yes "$ROOT/systemd/isuride-rust.service" "ubuntu@${ip}:/tmp/isuride-rust.service"
  "${SSH[@]}" "ubuntu@${ip}" "sudo env MATCHING=$matching bash -s" << 'EOF'
set -euo pipefail
rsync -a --delete --exclude target /tmp/isuride-rust/ /home/isucon/webapp/rust/
rsync -a --delete /tmp/isuride-sql/ /home/isucon/webapp/sql/
mkdir -p /home/isucon/webapp/rust/target/release
install -o isucon -g isucon -m 0755 /tmp/isuride-rust-bin /home/isucon/webapp/rust/target/release/isuride
chown -R isucon:isucon /home/isucon/webapp/rust /home/isucon/webapp/sql
cat > /home/isucon/env.sh << ENV
ISUCON_DB_HOST="192.168.0.212"
ISUCON_DB_PORT="3306"
ISUCON_DB_USER="isucon"
ISUCON_DB_PASSWORD="isucon"
ISUCON_DB_NAME="isuride"
ISUCON_MATCHING_INTERVAL=0.5
ISUCON_ENABLE_MATCHING="${MATCHING}"
ENV
chown isucon:isucon /home/isucon/env.sh
install -m 0644 /tmp/isuride-rust.service /etc/systemd/system/isuride-rust.service
systemctl daemon-reload
systemctl disable --now isuride-go.service || true
systemctl enable --now isuride-rust.service
systemctl restart isuride-rust.service || true
sleep 1
systemctl start isuride-rust.service
# local mysql no longer used
systemctl disable --now mysql || true
systemctl is-active isuride-rust
EOF
}

echo "==== mysql 192.168.0.212 ===="
scp -o BatchMode=yes "$ROOT/mysql/mysqld-isucon.cnf" ubuntu@192.168.0.212:/tmp/mysqld-isucon.cnf
"${SSH[@]}" ubuntu@192.168.0.212 'sudo bash -s' << 'EOF'
set -euo pipefail
install -m 0644 /tmp/mysqld-isucon.cnf /etc/mysql/mysql.conf.d/isucon.cnf
# Ubuntu default mysqld.cnf binds 127.0.0.1; comment it out so isucon.cnf wins.
if [ -f /etc/mysql/mysql.conf.d/mysqld.cnf ]; then
  sed -i 's/^bind-address/# bind-address/' /etc/mysql/mysql.conf.d/mysqld.cnf || true
  sed -i 's/^mysqlx-bind-address/# mysqlx-bind-address/' /etc/mysql/mysql.conf.d/mysqld.cnf || true
fi
systemctl disable --now isuride-rust isuride-go isuride-matcher || true
systemctl enable --now mysql
systemctl restart mysql
mysql -uroot -e "CREATE DATABASE IF NOT EXISTS isuride; CREATE USER IF NOT EXISTS 'isucon'@'%' IDENTIFIED BY 'isucon'; CREATE USER IF NOT EXISTS 'isucon'@'localhost' IDENTIFIED BY 'isucon'; GRANT ALL PRIVILEGES ON *.* TO 'isucon'@'%'; GRANT ALL PRIVILEGES ON *.* TO 'isucon'@'localhost'; FLUSH PRIVILEGES;"
ss -lntp | grep 3306 || true
EOF

deploy_app 192.168.0.211 1
deploy_app 192.168.0.210 0

echo "==== nginx LB 192.168.0.210 ===="
scp -o BatchMode=yes "$ROOT/nginx/isuride.conf" ubuntu@192.168.0.210:/tmp/isuride.conf
"${SSH[@]}" ubuntu@192.168.0.210 'sudo bash -s' << 'EOF'
set -euo pipefail
install -m 0644 /tmp/isuride.conf /etc/nginx/sites-available/isuride.conf
ln -sfn /etc/nginx/sites-available/isuride.conf /etc/nginx/sites-enabled/isuride.conf
nginx -t
systemctl reload nginx
systemctl disable --now isuride-matcher || true
EOF

"${SSH[@]}" ubuntu@192.168.0.211 'sudo systemctl enable --now isuride-matcher || true'
"${SSH[@]}" ubuntu@192.168.0.212 'sudo systemctl disable --now isuride-matcher || true'

echo cluster_deploy_done
