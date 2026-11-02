package main

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"net/http"
	"sync"
	"time"

	"github.com/oklog/ulid/v2"
)

type chairRideHint struct {
	hasRide              bool
	RideID               string
	PickupLatitude       int
	PickupLongitude      int
	DestinationLatitude  int
	DestinationLongitude int
}

var (
	chairPosMu sync.RWMutex
	chairPos   = map[string]ChairLatest{}

	chairRideMu sync.RWMutex
	chairRide   = map[string]chairRideHint{}
)

func rememberChairPos(chairID string, lat, lon int, at time.Time) {
	chairPosMu.Lock()
	prev, ok := chairPos[chairID]
	dist := 0
	if ok {
		dist = prev.TotalDistance + abs(lat-prev.Latitude) + abs(lon-prev.Longitude)
	}
	chairPos[chairID] = ChairLatest{
		Latitude:      lat,
		Longitude:     lon,
		TotalDistance: dist,
		UpdatedAt:     at,
	}
	chairPosMu.Unlock()
}

func getChairPos(chairID string) (ChairLatest, bool) {
	chairPosMu.RLock()
	pos, ok := chairPos[chairID]
	chairPosMu.RUnlock()
	return pos, ok
}

func rememberChairRide(chairID string, hint chairRideHint) {
	chairRideMu.Lock()
	chairRide[chairID] = hint
	chairRideMu.Unlock()
}

func getChairRideHint(chairID string) (chairRideHint, bool) {
	chairRideMu.RLock()
	hint, ok := chairRide[chairID]
	chairRideMu.RUnlock()
	return hint, ok
}

func clearMatchingCaches() {
	chairPosMu.Lock()
	chairPos = map[string]ChairLatest{}
	chairPosMu.Unlock()
	chairRideMu.Lock()
	chairRide = map[string]chairRideHint{}
	chairRideMu.Unlock()
}

type chairPostChairsRequest struct {
	Name               string `json:"name"`
	Model              string `json:"model"`
	ChairRegisterToken string `json:"chair_register_token"`
}

type chairPostChairsResponse struct {
	ID      string `json:"id"`
	OwnerID string `json:"owner_id"`
}

func chairPostChairs(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	req := &chairPostChairsRequest{}
	if err := bindJSON(r, req); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}
	if req.Name == "" || req.Model == "" || req.ChairRegisterToken == "" {
		writeError(w, http.StatusBadRequest, errors.New("some of required fields(name, model, chair_register_token) are empty"))
		return
	}

	owner := &Owner{}
	if err := db.GetContext(ctx, owner, "SELECT * FROM owners WHERE chair_register_token = ?", req.ChairRegisterToken); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			writeError(w, http.StatusUnauthorized, errors.New("invalid chair_register_token"))
			return
		}
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	chairID := ulid.Make().String()
	accessToken := secureRandomStr(32)

	_, err := db.ExecContext(
		ctx,
		"INSERT INTO chairs (id, owner_id, name, model, is_active, access_token) VALUES (?, ?, ?, ?, ?, ?)",
		chairID, owner.ID, req.Name, req.Model, false, accessToken,
	)
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	http.SetCookie(w, &http.Cookie{
		Path:  "/",
		Name:  "chair_session",
		Value: accessToken,
	})

	writeJSON(w, http.StatusCreated, &chairPostChairsResponse{
		ID:      chairID,
		OwnerID: owner.ID,
	})
}

type postChairActivityRequest struct {
	IsActive bool `json:"is_active"`
}

func chairPostActivity(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	chair := ctx.Value("chair").(*Chair)

	req := &postChairActivityRequest{}
	if err := bindJSON(r, req); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	_, err := db.ExecContext(ctx, "UPDATE chairs SET is_active = ? WHERE id = ?", req.IsActive, chair.ID)
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	w.WriteHeader(http.StatusNoContent)
}

type chairPostCoordinateResponse struct {
	RecordedAt int64 `json:"recorded_at"`
}

func chairPostCoordinate(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	req := &Coordinate{}
	if err := bindJSON(r, req); err != nil {
		w.WriteHeader(http.StatusBadRequest)
		return
	}

	chair := ctx.Value("chair").(*Chair)
	now := time.Now()
	// Wall-clock DATETIME so ordering matches MySQL CURRENT_TIMESTAMP / initial data.
	createdAt := now.Format("2006-01-02 15:04:05.000000")

	if _, err := db.ExecContext(
		ctx,
		`INSERT INTO chair_locations (id, chair_id, latitude, longitude, created_at) VALUES (?, ?, ?, ?, ?)`,
		ulid.Make().String(), chair.ID, req.Latitude, req.Longitude, createdAt,
	); err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	rememberChairPos(chair.ID, req.Latitude, req.Longitude, now)
	pos, _ := getChairPos(chair.ID)
	if _, err := db.ExecContext(
		ctx,
		`UPDATE chairs SET latitude = ?, longitude = ?, total_distance = ?, total_distance_updated_at = ? WHERE id = ?`,
		req.Latitude, req.Longitude, pos.TotalDistance, createdAt, chair.ID,
	); err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	if err := maybeArriveAtRidePoint(ctx, chair.ID, req); err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	writeJSON(w, http.StatusOK, &chairPostCoordinateResponse{
		RecordedAt: now.UnixMilli(),
	})
}

type currentChairRide struct {
	ID                   string `db:"id"`
	PickupLatitude       int    `db:"pickup_latitude"`
	PickupLongitude      int    `db:"pickup_longitude"`
	DestinationLatitude  int    `db:"destination_latitude"`
	DestinationLongitude int    `db:"destination_longitude"`
	Status               string `db:"status"`
}

func shouldCheckRideStatus(chairID string, coord *Coordinate) bool {
	hint, ok := getChairRideHint(chairID)
	if !ok {
		return true
	}
	if !hint.hasRide {
		return false
	}
	if coord.Latitude == hint.PickupLatitude && coord.Longitude == hint.PickupLongitude {
		return true
	}
	if coord.Latitude == hint.DestinationLatitude && coord.Longitude == hint.DestinationLongitude {
		return true
	}
	return false
}

func maybeArriveAtRidePoint(ctx context.Context, chairID string, coord *Coordinate) error {
	if !shouldCheckRideStatus(chairID, coord) {
		return nil
	}

	ride := currentChairRide{}
	err := db.GetContext(ctx, &ride, `
SELECT r.id, r.pickup_latitude, r.pickup_longitude, r.destination_latitude, r.destination_longitude,
       (SELECT rs.status FROM ride_statuses rs WHERE rs.ride_id = r.id ORDER BY rs.created_at DESC LIMIT 1) AS status
FROM rides r
WHERE r.chair_id = ?
ORDER BY r.updated_at DESC
LIMIT 1`, chairID)
	if err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			rememberChairRide(chairID, chairRideHint{})
			return nil
		}
		return err
	}

	if ride.Status == "COMPLETED" || ride.Status == "CANCELED" {
		hint, ok := getChairRideHint(chairID)
		if !ok || !hint.hasRide || hint.RideID == "" || hint.RideID == ride.ID {
			rememberChairRide(chairID, chairRideHint{})
		}
		return nil
	}

	rememberChairRide(chairID, chairRideHint{
		hasRide:              true,
		RideID:               ride.ID,
		PickupLatitude:       ride.PickupLatitude,
		PickupLongitude:      ride.PickupLongitude,
		DestinationLatitude:  ride.DestinationLatitude,
		DestinationLongitude: ride.DestinationLongitude,
	})

	if coord.Latitude == ride.PickupLatitude && coord.Longitude == ride.PickupLongitude && ride.Status == "ENROUTE" {
		if _, err := db.ExecContext(ctx, "INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)", ulid.Make().String(), ride.ID, "PICKUP"); err != nil {
			return err
		}
		return nil
	}

	if coord.Latitude == ride.DestinationLatitude && coord.Longitude == ride.DestinationLongitude && ride.Status == "CARRYING" {
		if _, err := db.ExecContext(ctx, "INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)", ulid.Make().String(), ride.ID, "ARRIVED"); err != nil {
			return err
		}
	}
	return nil
}

type simpleUser struct {
	ID   string `json:"id"`
	Name string `json:"name"`
}

type chairGetNotificationResponse struct {
	Data         *chairGetNotificationResponseData `json:"data"`
	RetryAfterMs int                               `json:"retry_after_ms"`
}

type chairGetNotificationResponseData struct {
	RideID                string     `json:"ride_id"`
	User                  simpleUser `json:"user"`
	PickupCoordinate      Coordinate `json:"pickup_coordinate"`
	DestinationCoordinate Coordinate `json:"destination_coordinate"`
	Status                string     `json:"status"`
}

func chairGetNotification(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	chair := ctx.Value("chair").(*Chair)

	data, unsent, err := fetchChairNotification(ctx, chair.ID)
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	if data == nil {
		writeJSON(w, http.StatusOK, &chairGetNotificationResponse{RetryAfterMs: 100})
		return
	}
	if unsent {
		if _, err := db.ExecContext(ctx, `UPDATE ride_statuses SET chair_sent_at = CURRENT_TIMESTAMP(6) WHERE ride_id = ? AND status = ? AND chair_sent_at IS NULL ORDER BY created_at ASC LIMIT 1`, data.RideID, data.Status); err != nil {
			writeError(w, http.StatusInternalServerError, err)
			return
		}
	}
	writeJSON(w, http.StatusOK, &chairGetNotificationResponse{Data: data, RetryAfterMs: 100})
}

func fetchChairNotification(ctx context.Context, chairID string) (*chairGetNotificationResponseData, bool, error) {
	tx, err := db.Beginx()
	if err != nil {
		return nil, false, err
	}
	defer tx.Rollback()

	yetSentRideStatus := RideStatus{}
	ride := Ride{}
	unsent := false

	err = tx.GetContext(ctx, &yetSentRideStatus, `
		SELECT rs.id, rs.ride_id, rs.status, rs.created_at
		FROM ride_statuses rs
		INNER JOIN rides r ON r.id = rs.ride_id
		WHERE r.chair_id = ? AND rs.chair_sent_at IS NULL
		ORDER BY rs.created_at ASC
		LIMIT 1`, chairID)
	if err == nil {
		unsent = true
		if err := tx.GetContext(ctx, &ride, `SELECT * FROM rides WHERE id = ?`, yetSentRideStatus.RideID); err != nil {
			return nil, false, err
		}
	} else if errors.Is(err, sql.ErrNoRows) {
		if err := tx.GetContext(ctx, &ride, `SELECT * FROM rides WHERE chair_id = ? ORDER BY updated_at DESC LIMIT 1`, chairID); err != nil {
			if errors.Is(err, sql.ErrNoRows) {
				return nil, false, nil
			}
			return nil, false, err
		}
		status, err := getLatestRideStatus(ctx, tx, ride.ID)
		if err != nil {
			return nil, false, err
		}
		yetSentRideStatus.Status = status
	} else {
		return nil, false, err
	}

	user := &User{}
	if err := tx.GetContext(ctx, user, "SELECT id, firstname, lastname FROM users WHERE id = ?", ride.UserID); err != nil {
		return nil, false, err
	}

	if err := tx.Commit(); err != nil {
		return nil, false, err
	}

	return &chairGetNotificationResponseData{
		RideID: ride.ID,
		User: simpleUser{
			ID:   user.ID,
			Name: fmt.Sprintf("%s %s", user.Firstname, user.Lastname),
		},
		PickupCoordinate: Coordinate{
			Latitude:  ride.PickupLatitude,
			Longitude: ride.PickupLongitude,
		},
		DestinationCoordinate: Coordinate{
			Latitude:  ride.DestinationLatitude,
			Longitude: ride.DestinationLongitude,
		},
		Status: yetSentRideStatus.Status,
	}, unsent, nil
}

type postChairRidesRideIDStatusRequest struct {
	Status string `json:"status"`
}

func chairPostRideStatus(w http.ResponseWriter, r *http.Request) {
	ctx := r.Context()
	rideID := r.PathValue("ride_id")

	chair := ctx.Value("chair").(*Chair)

	req := &postChairRidesRideIDStatusRequest{}
	if err := bindJSON(r, req); err != nil {
		writeError(w, http.StatusBadRequest, err)
		return
	}

	tx, err := db.Beginx()
	if err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}
	defer tx.Rollback()

	ride := &Ride{}
	if err := tx.GetContext(ctx, ride, "SELECT * FROM rides WHERE id = ? FOR UPDATE", rideID); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			writeError(w, http.StatusNotFound, errors.New("ride not found"))
			return
		}
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	if ride.ChairID.String != chair.ID {
		writeError(w, http.StatusBadRequest, errors.New("not assigned to this ride"))
		return
	}

	switch req.Status {
	// Acknowledge the ride
	case "ENROUTE":
		if _, err := tx.ExecContext(ctx, "INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)", ulid.Make().String(), ride.ID, "ENROUTE"); err != nil {
			writeError(w, http.StatusInternalServerError, err)
			return
		}
	// After Picking up user
	case "CARRYING":
		status, err := getLatestRideStatus(ctx, tx, ride.ID)
		if err != nil {
			writeError(w, http.StatusInternalServerError, err)
			return
		}
		if status != "PICKUP" {
			writeError(w, http.StatusBadRequest, errors.New("chair has not arrived yet"))
			return
		}
		if _, err := tx.ExecContext(ctx, "INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)", ulid.Make().String(), ride.ID, "CARRYING"); err != nil {
			writeError(w, http.StatusInternalServerError, err)
			return
		}
	default:
		writeError(w, http.StatusBadRequest, errors.New("invalid status"))
	}

	if err := tx.Commit(); err != nil {
		writeError(w, http.StatusInternalServerError, err)
		return
	}

	w.WriteHeader(http.StatusNoContent)
}
