package main

import (
	"context"
	"log/slog"
	"net/http"
	"sort"
	"sync"
	"time"

	"github.com/jmoiron/sqlx"
)

const matchingTick = 50 * time.Millisecond

var matchingMu sync.Mutex

func init() {
	go matchingLoop()
}

func matchingLoop() {
	for db == nil {
		time.Sleep(20 * time.Millisecond)
	}
	time.Sleep(100 * time.Millisecond)

	ticker := time.NewTicker(matchingTick)
	defer ticker.Stop()
	for range ticker.C {
		if matchingPaused.Load() {
			continue
		}
		if !matchingMu.TryLock() {
			continue
		}
		err := matchRidesLocked(context.Background())
		matchingMu.Unlock()
		if err != nil {
			slog.Warn("background matching failed", "err", err)
		}
	}
}

// internalGetMatching assigns idle chairs to unmatched rides.
// systemd isuride-matcher curls this; an in-process loop also calls the same matcher.
func internalGetMatching(w http.ResponseWriter, r *http.Request) {
	if matchingPaused.Load() {
		w.WriteHeader(http.StatusNoContent)
		return
	}
	matchingMu.Lock()
	err := matchRidesLocked(r.Context())
	matchingMu.Unlock()
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	w.WriteHeader(http.StatusNoContent)
}

type unmatchedRide struct {
	ID                   string    `db:"id"`
	PickupLatitude       int       `db:"pickup_latitude"`
	PickupLongitude      int       `db:"pickup_longitude"`
	DestinationLatitude  int       `db:"destination_latitude"`
	DestinationLongitude int       `db:"destination_longitude"`
	CreatedAt            time.Time `db:"created_at"`
}

type idleChair struct {
	ID    string `db:"id"`
	Speed int    `db:"speed"`
}

type chairLocRow struct {
	ChairID   string `db:"chair_id"`
	Latitude  int    `db:"latitude"`
	Longitude int    `db:"longitude"`
}

type matchPair struct {
	rideIdx  int
	chairIdx int
	dist     int
	eta      float64
}

func matchRidesLocked(ctx context.Context) error {
	rides := make([]unmatchedRide, 0, 64)
	if err := db.SelectContext(ctx, &rides, `
SELECT id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, created_at
FROM rides
WHERE chair_id IS NULL
ORDER BY created_at ASC`); err != nil {
		return err
	}
	if len(rides) == 0 {
		return nil
	}

	chairs := make([]idleChair, 0, 256)
	if err := db.SelectContext(ctx, &chairs, `
SELECT c.id, cm.speed
FROM chairs c
INNER JOIN chair_models cm ON cm.name = c.model
WHERE c.is_active = TRUE
  AND NOT EXISTS (
    SELECT 1 FROM rides r
    WHERE r.chair_id = c.id AND r.evaluation IS NULL
  )
  AND NOT EXISTS (
    SELECT 1 FROM rides r
    INNER JOIN ride_statuses rs ON rs.ride_id = r.id
      AND rs.status = 'COMPLETED' AND rs.chair_sent_at IS NULL
    WHERE r.chair_id = c.id
  )`); err != nil {
		return err
	}
	if len(chairs) == 0 {
		return nil
	}

	lats, lons, err := resolveChairPositions(ctx, chairs)
	if err != nil {
		return err
	}

	pairs := make([]matchPair, 0, len(rides)*8)
	for ri := range rides {
		for ci := range chairs {
			dist := 50000
			if lats[ci] != chairPosSentinel {
				dist = calculateDistance(lats[ci], lons[ci], rides[ri].PickupLatitude, rides[ri].PickupLongitude)
			}
			speed := chairs[ci].Speed
			if speed <= 0 {
				speed = 1
			}
			pairs = append(pairs, matchPair{
				rideIdx:  ri,
				chairIdx: ci,
				dist:     dist,
				eta:      float64(dist) / float64(speed),
			})
		}
	}
	if len(pairs) == 0 {
		return nil
	}

	sort.Slice(pairs, func(i, j int) bool {
		if pairs[i].eta != pairs[j].eta {
			return pairs[i].eta < pairs[j].eta
		}
		if pairs[i].dist != pairs[j].dist {
			return pairs[i].dist < pairs[j].dist
		}
		return rides[pairs[i].rideIdx].CreatedAt.Before(rides[pairs[j].rideIdx].CreatedAt)
	})

	rideUsed := make([]bool, len(rides))
	chairUsed := make([]bool, len(chairs))

	for _, p := range pairs {
		if rideUsed[p.rideIdx] || chairUsed[p.chairIdx] {
			continue
		}

		chairID := chairs[p.chairIdx].ID
		ride := rides[p.rideIdx]
		result, err := db.ExecContext(ctx,
			`UPDATE rides SET chair_id = ?
			 WHERE id = ? AND chair_id IS NULL
			   AND NOT EXISTS (
			     SELECT 1 FROM (
			       SELECT id FROM rides WHERE chair_id = ? AND evaluation IS NULL LIMIT 1
			     ) busy
			   )
			   AND NOT EXISTS (
			     SELECT 1 FROM (
			       SELECT r.id FROM rides r
			       INNER JOIN ride_statuses rs ON rs.ride_id = r.id
			         AND rs.status = 'COMPLETED' AND rs.chair_sent_at IS NULL
			       WHERE r.chair_id = ? LIMIT 1
			     ) unsent
			   )`,
			chairID, ride.ID, chairID, chairID,
		)
		if err != nil {
			return err
		}
		n, err := result.RowsAffected()
		if err != nil {
			return err
		}
		if n == 0 {
			continue
		}

		rideUsed[p.rideIdx] = true
		chairUsed[p.chairIdx] = true
		rememberChairRide(chairID, chairRideHint{
			hasRide:              true,
			RideID:               ride.ID,
			PickupLatitude:       ride.PickupLatitude,
			PickupLongitude:      ride.PickupLongitude,
			DestinationLatitude:  ride.DestinationLatitude,
			DestinationLongitude: ride.DestinationLongitude,
		})
	}

	return nil
}

const chairPosSentinel = -1 << 30

func resolveChairPositions(ctx context.Context, chairs []idleChair) (lats, lons []int, err error) {
	lats = make([]int, len(chairs))
	lons = make([]int, len(chairs))
	missing := make([]string, 0, 16)
	missingIdx := make(map[string][]int, 16)

	for i, c := range chairs {
		if pos, ok := getChairPos(c.ID); ok {
			lats[i] = pos.Latitude
			lons[i] = pos.Longitude
			continue
		}
		lats[i] = chairPosSentinel
		lons[i] = chairPosSentinel
		if _, seen := missingIdx[c.ID]; !seen {
			missing = append(missing, c.ID)
		}
		missingIdx[c.ID] = append(missingIdx[c.ID], i)
	}
	if len(missing) == 0 {
		return lats, lons, nil
	}

	query, args, err := sqlx.In(`
SELECT cl.chair_id, cl.latitude, cl.longitude
FROM chair_locations cl
INNER JOIN (
  SELECT chair_id, MAX(created_at) AS created_at
  FROM chair_locations
  WHERE chair_id IN (?)
  GROUP BY chair_id
) latest ON latest.chair_id = cl.chair_id AND latest.created_at = cl.created_at`, missing)
	if err != nil {
		return nil, nil, err
	}
	var rows []chairLocRow
	if err := db.SelectContext(ctx, &rows, query, args...); err != nil {
		return nil, nil, err
	}
	now := time.Now()
	for _, row := range rows {
		rememberChairPos(row.ChairID, row.Latitude, row.Longitude, now)
		for _, i := range missingIdx[row.ChairID] {
			lats[i] = row.Latitude
			lons[i] = row.Longitude
		}
	}
	return lats, lons, nil
}
