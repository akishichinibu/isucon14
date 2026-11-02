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
    if !state.matching_enabled {
        return Ok(());
    }
    let _guard = state.matching_lock.lock().await;
    match_inner(state).await
}

pub async fn try_run_matching(state: &AppState) -> Result<(), Error> {
    if !state.matching_enabled {
        return Ok(());
    }
    let Ok(_guard) = state.matching_lock.try_lock() else {
        return Ok(());
    };
    match_inner(state).await
}

async fn match_inner(state: &AppState) -> Result<(), Error> {
    let assignments = {
        let mut world = state.world.write().unwrap();
        world.match_once()
    };

    for (ride_id, chair_id, hint) in assignments {
        let result = sqlx::query("UPDATE rides SET chair_id = ? WHERE id = ? AND chair_id IS NULL")
            .bind(&chair_id)
            .bind(&ride_id)
            .execute(&state.pool)
            .await?;
        if result.rows_affected() == 0 {
            // Lost the race; free the chair in memory.
            let mut world = state.world.write().unwrap();
            if let Some(c) = world.chairs.get_mut(&chair_id) {
                if c.current_ride
                    .as_ref()
                    .map(|h| h.ride_id == ride_id)
                    .unwrap_or(false)
                {
                    c.current_ride = None;
                }
            }
            world.unmatched.push_front(crate::state::UnmatchedRide {
                id: ride_id,
                pickup_latitude: hint.pickup_latitude,
                pickup_longitude: hint.pickup_longitude,
                destination_latitude: hint.destination_latitude,
                destination_longitude: hint.destination_longitude,
                created_at: chrono::Utc::now(),
            });
        }
    }

    Ok(())
}
