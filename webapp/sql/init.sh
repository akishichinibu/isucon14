#!/usr/bin/env bash

set -eux
cd $(dirname $0)

if [ "${ENV:-}" == "local-dev" ]; then
  exit 0
fi

if test -f /home/isucon/env.sh; then
	. /home/isucon/env.sh
fi

ISUCON_DB_HOST=${ISUCON_DB_HOST:-127.0.0.1}
ISUCON_DB_PORT=${ISUCON_DB_PORT:-3306}
ISUCON_DB_USER=${ISUCON_DB_USER:-isucon}
ISUCON_DB_PASSWORD=${ISUCON_DB_PASSWORD:-isucon}
ISUCON_DB_NAME=${ISUCON_DB_NAME:-isuride}

mysql_cmd() {
	mysql -u"$ISUCON_DB_USER" \
		-p"$ISUCON_DB_PASSWORD" \
		--host "$ISUCON_DB_HOST" \
		--port "$ISUCON_DB_PORT" \
		"$ISUCON_DB_NAME" "$@"
}

# MySQLを初期化
mysql_cmd < 1-schema.sql

mysql_cmd < 2-master-data.sql

# 3-initial-data.sql.gz is mysqldump positional INSERT matching the original
# column lists. Extra denormalized columns would make those INSERT statements
# fail, so drop them for the load and add them back (same defs as 1-schema.sql).
mysql_cmd <<'SQL'
ALTER TABLE chairs
  DROP COLUMN latitude,
  DROP COLUMN longitude,
  DROP COLUMN total_distance,
  DROP COLUMN total_distance_updated_at;
ALTER TABLE rides
  DROP COLUMN latest_status;
SQL

gzip -dkc 3-initial-data.sql.gz | mysql_cmd

mysql_cmd <<'SQL'
ALTER TABLE chairs
  ADD COLUMN latitude INTEGER NULL COMMENT 'latest latitude from chair_locations' AFTER updated_at,
  ADD COLUMN longitude INTEGER NULL COMMENT 'latest longitude from chair_locations' AFTER latitude,
  ADD COLUMN total_distance INTEGER NOT NULL DEFAULT 0 COMMENT 'cumulative manhattan distance' AFTER longitude,
  ADD COLUMN total_distance_updated_at DATETIME(6) NULL COMMENT 'when total_distance was last updated' AFTER total_distance;
ALTER TABLE rides
  ADD COLUMN latest_status VARCHAR(16) NULL DEFAULT NULL COMMENT 'latest ride_statuses.status' AFTER updated_at;
SQL
