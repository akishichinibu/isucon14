use axum::extract::State;
use axum::http::StatusCode;

use crate::{AppState, Error};

pub fn internal_routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/api/internal/matching",
        axum::routing::get(internal_get_matching),
    )
}

async fn internal_get_matching(State(state): State<AppState>) -> Result<StatusCode, Error> {
    run_matching(&state).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn run_matching(state: &AppState) -> Result<(), Error> {
    let _guard = state.matching_lock.lock().await;
    match_inner(&state.pool).await
}

pub async fn try_run_matching(state: &AppState) -> Result<(), Error> {
    let Ok(_guard) = state.matching_lock.try_lock() else {
        return Ok(());
    };
    match_inner(&state.pool).await
}

#[derive(Debug, sqlx::FromRow)]
struct UnmatchedRide {
    id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
}

#[derive(Debug, sqlx::FromRow)]
struct IdleChair {
    id: String,
    latitude: i32,
    longitude: i32,
    speed: i32,
}

/// Assign unmatched rides to the idle chair with the lowest ETA
/// (manhattan distance / model speed). Oldest rides first.
async fn match_inner(pool: &sqlx::MySqlPool) -> Result<(), Error> {
    let rides: Vec<UnmatchedRide> = sqlx::query_as(
        "SELECT id, pickup_latitude, pickup_longitude FROM rides WHERE chair_id IS NULL ORDER BY created_at",
    )
    .fetch_all(pool)
    .await?;
    if rides.is_empty() {
        return Ok(());
    }

    let mut chairs: Vec<IdleChair> = sqlx::query_as(
        r#"SELECT c.id, c.latitude, c.longitude, cm.speed
           FROM chairs c
           INNER JOIN chair_models cm ON cm.name = c.model
           WHERE c.is_active = TRUE
             AND c.latitude IS NOT NULL
             AND c.longitude IS NOT NULL
             AND NOT EXISTS (
               SELECT 1 FROM rides r
               WHERE r.chair_id = c.id AND r.latest_status <> 'COMPLETED'
             )"#,
    )
    .fetch_all(pool)
    .await?;
    if chairs.is_empty() {
        return Ok(());
    }

    let mut assignments: Vec<(String, String)> = Vec::new();
    for ride in rides {
        let mut best_idx = None;
        let mut best_eta = f64::INFINITY;
        for (idx, chair) in chairs.iter().enumerate() {
            if chair.speed <= 0 {
                continue;
            }
            let distance = crate::calculate_distance(
                ride.pickup_latitude,
                ride.pickup_longitude,
                chair.latitude,
                chair.longitude,
            );
            let eta = distance as f64 / chair.speed as f64;
            if eta < best_eta {
                best_eta = eta;
                best_idx = Some(idx);
            }
        }
        if let Some(idx) = best_idx {
            let chair = chairs.swap_remove(idx);
            assignments.push((ride.id, chair.id));
        }
        if chairs.is_empty() {
            break;
        }
    }

    for (ride_id, chair_id) in assignments {
        sqlx::query("UPDATE rides SET chair_id = ? WHERE id = ? AND chair_id IS NULL")
            .bind(&chair_id)
            .bind(&ride_id)
            .execute(pool)
            .await?;
    }

    Ok(())
}
