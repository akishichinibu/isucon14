# Schema notes for app agents (feat/db)

`webapp/sql/1-schema.sql` is DROP+CREATE. `init.sh` still loads `3-initial-data.sql.gz`.
Denormalized columns are dropped around that dump (positional INSERT) and added back.

sqlx `SELECT *` is strict. Add these fields to structs **before** deploying this schema
or middleware / nearby / matching will fail with "missing destination name".

## chairs (denormalized; INSERT may omit these)

| Column | Type | Default | Written by |
|---|---|---|---|
| `latitude` | INT NULL | NULL | `POST /api/chair/coordinate` — latest location |
| `longitude` | INT NULL | NULL | same |
| `total_distance` | INT NOT NULL | 0 | same — add manhattan step from previous lat/lng |
| `total_distance_updated_at` | DATETIME(6) NULL | NULL | same — location `created_at` |

Go: `sql.NullInt64` / `sql.NullTime` (or `*int` / `*time.Time`) plus `db` tags.
Do **not** bump these only in SQL window functions; owner `GET /api/owner/chairs` and
nearby-chairs 3s grace (CODE=26) need the columns maintained on every coordinate POST.

Suggested writes (same transaction as `INSERT INTO chair_locations`):

```sql
UPDATE chairs
SET latitude = ?,
    longitude = ?,
    total_distance = total_distance + ?,
    total_distance_updated_at = ?
WHERE id = ?;
```

`?` distance = `ABS(new_lat - old_lat) + ABS(new_lng - old_lng)` when old lat/lng are non-NULL, else 0.
After that, owner chairs can `SELECT ... total_distance, total_distance_updated_at FROM chairs WHERE owner_id = ?`
and drop the `chair_locations` window JOIN. Nearby can filter `is_active = TRUE AND latitude IS NOT NULL`.

Dump chairs start with NULL lat/lng and `total_distance = 0` until the first coordinate POST.

## rides (optional denorm)

| Column | Type | Default | Written by |
|---|---|---|---|
| `latest_status` | VARCHAR(16) NULL | NULL | every `INSERT INTO ride_statuses` |

Keep it in sync (`MATCHING`/`ENROUTE`/`PICKUP`/`CARRYING`/`ARRIVED`/`COMPLETED`).
Then `getLatestRideStatus` and the matching "chair empty?" COUNT can read `rides.latest_status`
instead of scanning `ride_statuses`. Add `sql.NullString` `db:"latest_status"` on `Ride`.

## Indexes (already in CREATE TABLE)

- `chairs`: UNIQUE `access_token`; KEY `is_active`; KEY `owner_id`
- `chair_locations`: `(chair_id, created_at)` — latest location `ORDER BY created_at DESC LIMIT 1`
- `rides`: `(user_id, created_at)`; `(chair_id, updated_at)`; `(chair_id, created_at)` for unmatched `chair_id IS NULL ORDER BY created_at` and nearby
- `ride_statuses`: `(ride_id, created_at)`; `(ride_id, app_sent_at, created_at)`; `(ride_id, chair_sent_at, created_at)`
- `coupons`: `(used_by)`

## MySQL drop-in

`mysql/mysqld-isucon.cnf` — copy to `/etc/mysql/mysql.conf.d/isucon.cnf` on the 4GiB guest (not applied by init.sh).
