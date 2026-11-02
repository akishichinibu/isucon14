use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum_extra::extract::cookie::Cookie;
use axum_extra::extract::CookieJar;
use chrono::{DateTime, Utc};
use ulid::Ulid;

use crate::models::{Chair, Owner};
use crate::{
    insert_ride_status, AppState, Coordinate, Error, NOTIFICATION_RETRY_AFTER_MS,
};

pub fn chair_routes(app_state: AppState) -> axum::Router<AppState> {
    let routes =
        axum::Router::new().route("/api/chair/chairs", axum::routing::post(chair_post_chairs));

    let authed_routes = axum::Router::new()
        .route(
            "/api/chair/activity",
            axum::routing::post(chair_post_activity),
        )
        .route(
            "/api/chair/coordinate",
            axum::routing::post(chair_post_coordinate),
        )
        .route(
            "/api/chair/notification",
            axum::routing::get(chair_get_notification),
        )
        .route(
            "/api/chair/rides/:ride_id/status",
            axum::routing::post(chair_post_ride_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::middlewares::chair_auth_middleware,
        ));

    routes.merge(authed_routes)
}

#[derive(Debug, serde::Deserialize)]
struct ChairPostChairsRequest {
    name: String,
    model: String,
    chair_register_token: String,
}

#[derive(Debug, serde::Serialize)]
struct ChairPostChairsResponse {
    id: String,
    owner_id: String,
}

async fn chair_post_chairs(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::Json(req): axum::Json<ChairPostChairsRequest>,
) -> Result<(CookieJar, (StatusCode, axum::Json<ChairPostChairsResponse>)), Error> {
    let Some(owner): Option<Owner> =
        sqlx::query_as("SELECT id, name, access_token, created_at, updated_at FROM owners WHERE chair_register_token = ?")
            .bind(req.chair_register_token)
            .fetch_optional(&state.pool)
            .await?
    else {
        return Err(Error::Unauthorized("invalid chair_register_token"));
    };

    let chair_id = Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let speed: i32 = sqlx::query_scalar("SELECT speed FROM chair_models WHERE name = ?")
        .bind(&req.model)
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or(1);

    sqlx::query("INSERT INTO chairs (id, owner_id, name, model, is_active, access_token) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&chair_id)
        .bind(&owner.id)
        .bind(&req.name)
        .bind(&req.model)
        .bind(false)
        .bind(&access_token)
        .execute(&state.pool)
        .await?;

    state.world.write().unwrap().upsert_chair(crate::state::ChairMem {
        id: chair_id.clone(),
        owner_id: owner.id.clone(),
        name: req.name,
        model: req.model,
        speed: speed.max(1),
        is_active: false,
        created_at: Utc::now(),
        latitude: None,
        longitude: None,
        total_distance: 0,
        total_distance_updated_at: None,
        current_ride: None,
        completed_unsent: false,
    });

    let jar = jar.add(Cookie::build(("chair_session", access_token)).path("/"));

    Ok((
        jar,
        (
            StatusCode::CREATED,
            axum::Json(ChairPostChairsResponse {
                id: chair_id,
                owner_id: owner.id,
            }),
        ),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct PostChairActivityRequest {
    is_active: bool,
}

async fn chair_post_activity(
    State(state): State<AppState>,
    axum::Extension(chair): axum::Extension<Chair>,
    axum::Json(req): axum::Json<PostChairActivityRequest>,
) -> Result<StatusCode, Error> {
    sqlx::query("UPDATE chairs SET is_active = ? WHERE id = ?")
        .bind(req.is_active)
        .bind(&chair.id)
        .execute(&state.pool)
        .await?;
    state.world.write().unwrap().set_active(&chair.id, req.is_active);

    if req.is_active {
        crate::spawn_matching(state);
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Serialize)]
struct ChairPostCoordinateResponse {
    recorded_at: i64,
}

#[derive(Debug, sqlx::FromRow)]
struct CurrentRide {
    id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
    latest_status: String,
}

async fn chair_post_coordinate(
    State(state): State<AppState>,
    axum::Extension(chair): axum::Extension<Chair>,
    axum::Json(req): axum::Json<Coordinate>,
) -> Result<axum::Json<ChairPostCoordinateResponse>, Error> {
    let now = Utc::now();
    let transition = state
        .world
        .write()
        .unwrap()
        .update_coord(&chair.id, req.latitude, req.longitude, now);

    if let Some(hint) = transition {
        if hint.status == "PICKUP" || hint.status == "ARRIVED" {
            let mut tx = state.pool.begin().await?;
            insert_ride_status(&mut tx, &hint.ride_id, &hint.status).await?;
            tx.commit().await?;
        }
    }

    let pool = state.pool.clone();
    let chair_id = chair.id.clone();
    let lat = req.latitude;
    let lng = req.longitude;
    tokio::spawn(async move {
        let loc_id = Ulid::new().to_string();
        let _ = sqlx::query(
            "INSERT INTO chair_locations (id, chair_id, latitude, longitude) VALUES (?, ?, ?, ?)",
        )
        .bind(&loc_id)
        .bind(&chair_id)
        .bind(lat)
        .bind(lng)
        .execute(&pool)
        .await;
        let _ = sqlx::query(
            r#"UPDATE chairs
               SET total_distance = total_distance + IF(latitude IS NULL, 0, ABS(latitude - ?) + ABS(longitude - ?)),
                   latitude = ?,
                   longitude = ?,
                   total_distance_updated_at = CURRENT_TIMESTAMP(6),
                   updated_at = updated_at
               WHERE id = ?"#,
        )
        .bind(lat)
        .bind(lng)
        .bind(lat)
        .bind(lng)
        .bind(&chair_id)
        .execute(&pool)
        .await;
    });

    Ok(axum::Json(ChairPostCoordinateResponse {
        recorded_at: now.timestamp_millis(),
    }))
}

#[derive(Debug, serde::Serialize)]
struct SimpleUser {
    id: String,
    name: String,
}

#[derive(Debug, serde::Serialize)]
struct ChairGetNotificationResponse {
    data: Option<ChairGetNotificationResponseData>,
    retry_after_ms: i32,
}

#[derive(Debug, serde::Serialize)]
struct ChairGetNotificationResponseData {
    ride_id: String,
    user: SimpleUser,
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
    status: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ChairRideRow {
    id: String,
    user_id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
    latest_status: String,
}

async fn chair_get_notification(
    State(state): State<AppState>,
    axum::Extension(chair): axum::Extension<Chair>,
) -> Result<axum::Json<ChairGetNotificationResponse>, Error> {
    let mut tx = state.pool.begin().await?;

    // Drain unsent statuses across all rides for this chair, oldest first,
    // so COMPLETED of the previous ride is delivered before MATCHING of the next.
    let yet_sent: Option<(String, String, String)> = sqlx::query_as(
        r#"SELECT rs.id, rs.status, rs.ride_id
           FROM ride_statuses rs
           INNER JOIN rides r ON r.id = rs.ride_id
           WHERE r.chair_id = ? AND rs.chair_sent_at IS NULL
           ORDER BY rs.created_at ASC
           LIMIT 1"#,
    )
    .bind(&chair.id)
    .fetch_optional(&mut *tx)
    .await?;

    let ride: Option<ChairRideRow> = if let Some((_, _, ride_id)) = &yet_sent {
        sqlx::query_as(
            r#"SELECT id, user_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, latest_status
               FROM rides WHERE id = ?"#,
        )
        .bind(ride_id)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        sqlx::query_as(
            r#"SELECT id, user_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, latest_status
               FROM rides WHERE chair_id = ? ORDER BY updated_at DESC LIMIT 1"#,
        )
        .bind(&chair.id)
        .fetch_optional(&mut *tx)
        .await?
    };
    let Some(ride) = ride else {
        return Ok(axum::Json(ChairGetNotificationResponse {
            data: None,
            retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
        }));
    };

    let (yet_sent_id, status) = if let Some((id, status, _)) = yet_sent {
        (Some(id), status)
    } else {
        (None, ride.latest_status)
    };

    let user: (String, String, String) =
        sqlx::query_as("SELECT id, firstname, lastname FROM users WHERE id = ?")
            .bind(&ride.user_id)
            .fetch_one(&mut *tx)
            .await?;

    if let Some(yet_sent_id) = yet_sent_id {
        sqlx::query("UPDATE ride_statuses SET chair_sent_at = CURRENT_TIMESTAMP(6) WHERE id = ?")
            .bind(yet_sent_id)
            .execute(&mut *tx)
            .await?;
        if status == "COMPLETED" {
            state.world.write().unwrap().mark_completed_sent(&chair.id);
        }
    }

    tx.commit().await?;

    Ok(axum::Json(ChairGetNotificationResponse {
        data: Some(ChairGetNotificationResponseData {
            ride_id: ride.id,
            user: SimpleUser {
                id: user.0,
                name: format!("{} {}", user.1, user.2),
            },
            pickup_coordinate: Coordinate {
                latitude: ride.pickup_latitude,
                longitude: ride.pickup_longitude,
            },
            destination_coordinate: Coordinate {
                latitude: ride.destination_latitude,
                longitude: ride.destination_longitude,
            },
            status,
        }),
        retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct PostChairRidesRideIDStatusRequest {
    status: String,
}

async fn chair_post_ride_status(
    State(state): State<AppState>,
    axum::Extension(chair): axum::Extension<Chair>,
    Path((ride_id,)): Path<(String,)>,
    axum::Json(req): axum::Json<PostChairRidesRideIDStatusRequest>,
) -> Result<StatusCode, Error> {
    let mut tx = state.pool.begin().await?;

    let Some(ride): Option<(Option<String>, String)> =
        sqlx::query_as("SELECT chair_id, latest_status FROM rides WHERE id = ? FOR UPDATE")
            .bind(&ride_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        return Err(Error::NotFound("rides not found"));
    };

    if ride.0.as_deref() != Some(chair.id.as_str()) {
        return Err(Error::BadRequest("not assigned to this ride"));
    }

    match req.status.as_str() {
        "ENROUTE" => {
            insert_ride_status(&mut tx, &ride_id, "ENROUTE").await?;
        }
        "CARRYING" => {
            if ride.1 != "PICKUP" {
                return Err(Error::BadRequest("chair has not arrived yet"));
            }
            insert_ride_status(&mut tx, &ride_id, "CARRYING").await?;
        }
        _ => {
            return Err(Error::BadRequest("invalid status"));
        }
    };

    tx.commit().await?;

    if req.status == "ENROUTE" || req.status == "CARRYING" {
        let mut world = state.world.write().unwrap();
        if let Some(c) = world.chairs.get_mut(&chair.id) {
            if let Some(ride) = c.current_ride.as_mut() {
                if ride.ride_id == ride_id {
                    ride.status = req.status;
                }
            }
        }
    }

    Ok(StatusCode::NO_CONTENT)
}
