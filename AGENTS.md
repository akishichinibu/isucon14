# isuride (ISUCON14)

Homelab practice on t5820. Baseline score 2042, Go reference.

## Constraints

- `POST /api/initialize` must stay correct: run `webapp/sql/init.sh`, then set `payment_gateway_url`. Timeout 30s.
- Matching grace 30s. Coordinate / nearby / owner total_distance grace 3s (CODE=26).
- Notification within 3s. JSON polling or SSE both allowed.
- Manhattan distance. Fare `500 + dist*100` minus coupons.
- Do not talk to the bench VM except as payment server URL given at initialize.
- Language field in initialize response must match the running implementation (`go` or `rust`).
- Comments in English.

## Hot paths (reference)

- `GET /api/internal/matching` every 0.5s: `ORDER BY RAND()`, nested COUNT on ride_statuses.
- `POST /api/chair/coordinate`: insert-only `chair_locations` + latest-ride status.
- `GET /api/app/notification` and `GET /api/chair/notification`: 30ms poll, extra queries per call.
- `GET /api/owner/chairs`: window function over all `chair_locations`.
- No indexes besides PKs/UNIQUEs. Default sqlx pool. chi request logger on.

## Deploy / bench (orchestrator only)

Subagents do not SSH to the VMs. Commit on the current branch.
Orchestrator runs `bash scripts/deploy.sh` then `bash scripts/run-bench.sh`.
