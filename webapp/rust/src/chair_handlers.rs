use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum_extra::extract::cookie::Cookie;
use axum_extra::extract::CookieJar;
use chrono::{DateTime, Utc};
use ulid::Ulid;

use crate::models::{Chair, Owner};
use crate::{AppState, Coordinate, Error, NOTIFICATION_RETRY_AFTER_MS};

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
        total_rides_count: 0,
        total_evaluation_sum: 0,
        accrued_sales: 0,
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
            crate::state::persist_status(state.pool.clone(), hint.ride_id, hint.status);
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
    let snapshot = {
        let mut world = state.world.write().unwrap();
        world
            .pop_chair_event(&chair.id)
            .or_else(|| world.peek_chair_snapshot(&chair.id))
    };
    let Some(ev) = snapshot else {
        return Ok(axum::Json(ChairGetNotificationResponse {
            data: None,
            retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
        }));
    };

    let world = state.world.read().unwrap();
    let Some(ride) = world.rides.get(&ev.ride_id) else {
        return Ok(axum::Json(ChairGetNotificationResponse {
            data: None,
            retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
        }));
    };
    let user = world.users.get(&ride.user_id);
    let name = user
        .map(|u| format!("{} {}", u.firstname, u.lastname))
        .unwrap_or_default();
    let data = ChairGetNotificationResponseData {
        ride_id: ride.id.clone(),
        user: SimpleUser {
            id: ride.user_id.clone(),
            name,
        },
        pickup_coordinate: Coordinate {
            latitude: ride.pickup_latitude,
            longitude: ride.pickup_longitude,
        },
        destination_coordinate: Coordinate {
            latitude: ride.destination_latitude,
            longitude: ride.destination_longitude,
        },
        status: ev.status,
    };
    drop(world);

    Ok(axum::Json(ChairGetNotificationResponse {
        data: Some(data),
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
    {
        let mut world = state.world.write().unwrap();
        let current = world
            .chairs
            .get(&chair.id)
            .and_then(|c| c.current_ride.clone());
        let Some(current) = current else {
            return Err(Error::BadRequest("not assigned to this ride"));
        };
        if current.ride_id != ride_id {
            return Err(Error::BadRequest("not assigned to this ride"));
        }
        match req.status.as_str() {
            "ENROUTE" => world.push_status(&ride_id, "ENROUTE"),
            "CARRYING" => {
                if current.status != "PICKUP" {
                    return Err(Error::BadRequest("chair has not arrived yet"));
                }
                world.push_status(&ride_id, "CARRYING");
            }
            _ => return Err(Error::BadRequest("invalid status")),
        }
    }
    crate::state::persist_status(state.pool.clone(), ride_id, req.status);
    Ok(StatusCode::NO_CONTENT)
}
