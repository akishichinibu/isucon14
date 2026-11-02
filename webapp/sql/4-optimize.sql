SET CHARACTER_SET_CLIENT = utf8mb4;
SET CHARACTER_SET_CONNECTION = utf8mb4;

USE isuride;

-- Extra columns are added AFTER the initial dump so INSERT INTO ... VALUES (...)
-- in 3-initial-data.sql.gz still matches the original column lists.
-- Defaults keep Go named INSERTs working.

ALTER TABLE chairs
  ADD COLUMN latitude INTEGER NULL DEFAULT NULL COMMENT 'latest latitude',
  ADD COLUMN longitude INTEGER NULL DEFAULT NULL COMMENT 'latest longitude',
  ADD COLUMN total_distance INTEGER NOT NULL DEFAULT 0 COMMENT 'cumulative manhattan distance',
  ADD COLUMN total_distance_updated_at DATETIME(6) NULL DEFAULT NULL COMMENT 'when total_distance was last updated',
  ADD COLUMN total_rides_count INTEGER NOT NULL DEFAULT 0 COMMENT 'completed rides',
  ADD COLUMN total_evaluation_sum INTEGER NOT NULL DEFAULT 0 COMMENT 'sum of evaluations';

ALTER TABLE rides
  ADD COLUMN latest_status ENUM ('MATCHING', 'ENROUTE', 'PICKUP', 'CARRYING', 'ARRIVED', 'COMPLETED') NOT NULL DEFAULT 'MATCHING' COMMENT 'denormalized latest ride status',
  ADD INDEX idx_rides_chair_latest (chair_id, latest_status);

-- Latest chair coordinates
UPDATE chairs c
INNER JOIN (
  SELECT cl.chair_id, cl.latitude, cl.longitude, cl.created_at
  FROM chair_locations cl
  INNER JOIN (
    SELECT chair_id, MAX(created_at) AS max_created_at
    FROM chair_locations
    GROUP BY chair_id
  ) t ON t.chair_id = cl.chair_id AND t.max_created_at = cl.created_at
) loc ON loc.chair_id = c.id
SET c.latitude = loc.latitude,
    c.longitude = loc.longitude,
    c.total_distance_updated_at = loc.created_at,
    c.updated_at = c.updated_at;

-- Cumulative distance from consecutive location samples
UPDATE chairs c
INNER JOIN (
  SELECT chair_id,
         IFNULL(SUM(distance), 0) AS total_distance,
         MAX(created_at) AS updated_at
  FROM (
    SELECT chair_id,
           created_at,
           ABS(latitude - LAG(latitude) OVER (PARTITION BY chair_id ORDER BY created_at)) +
           ABS(longitude - LAG(longitude) OVER (PARTITION BY chair_id ORDER BY created_at)) AS distance
    FROM chair_locations
  ) tmp
  GROUP BY chair_id
) d ON d.chair_id = c.id
SET c.total_distance = d.total_distance,
    c.total_distance_updated_at = d.updated_at,
    c.updated_at = c.updated_at;

-- Latest ride status
UPDATE rides r
INNER JOIN (
  SELECT rs.ride_id, rs.status
  FROM ride_statuses rs
  INNER JOIN (
    SELECT ride_id, MAX(created_at) AS max_created_at
    FROM ride_statuses
    GROUP BY ride_id
  ) t ON t.ride_id = rs.ride_id AND t.max_created_at = rs.created_at
) latest ON latest.ride_id = r.id
SET r.latest_status = latest.status,
    r.updated_at = r.updated_at;

-- Chair evaluation stats for completed rides
UPDATE chairs c
INNER JOIN (
  SELECT chair_id,
         COUNT(*) AS cnt,
         IFNULL(SUM(evaluation), 0) AS eval_sum
  FROM rides
  WHERE latest_status = 'COMPLETED'
    AND evaluation IS NOT NULL
  GROUP BY chair_id
) s ON s.chair_id = c.id
SET c.total_rides_count = s.cnt,
    c.total_evaluation_sum = s.eval_sum,
    c.updated_at = c.updated_at;
