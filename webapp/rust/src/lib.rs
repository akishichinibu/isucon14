use std::sync::Arc;
use ulid::Ulid;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::MySqlPool,
    pub matching_lock: Arc<tokio::sync::Mutex<()>>,
    pub world: crate::state::SharedWorld,
    pub matching_enabled: bool,
    pub static_files: Arc<std::collections::HashMap<String, (String, Vec<u8>)>>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("failed to initialize: stdout={stdout} stderr={stderr}")]
    Initialize { stdout: String, stderr: String },
    #[error("{0}")]
    PaymentGateway(#[from] crate::payment_gateway::PaymentGatewayError),
    #[error("{0}")]
    BadRequest(&'static str),
    #[error("{0}")]
    Unauthorized(&'static str),
    #[error("{0}")]
    NotFound(&'static str),
    #[error("{0}")]
    Conflict(&'static str),
}

#[derive(Debug, Clone, Copy)]
pub struct Coordinate {
    pub latitude: i32,
    pub longitude: i32,
}

pub async fn require_user(state: &AppState, token: Option<&str>) -> Result<String, Error> {
    let t = token.ok_or(Error::Unauthorized("app_session cookie is required"))?;
    state
        .world
        .read()
        .unwrap()
        .user_id_by_token(t)
        .map(str::to_string)
        .ok_or(Error::Unauthorized("invalid access token"))
}

pub async fn require_chair(state: &AppState, token: Option<&str>) -> Result<String, Error> {
    let t = token.ok_or(Error::Unauthorized("chair_session cookie is required"))?;
    state
        .world
        .read()
        .unwrap()
        .chair_id_by_token(t)
        .map(str::to_string)
        .ok_or(Error::Unauthorized("invalid access token"))
}

pub async fn require_owner(state: &AppState, token: Option<&str>) -> Result<String, Error> {
    let t = token.ok_or(Error::Unauthorized("owner_session cookie is required"))?;
    state
        .world
        .read()
        .unwrap()
        .owner_id_by_token(t)
        .map(str::to_string)
        .ok_or(Error::Unauthorized("invalid access token"))
}

pub fn secure_random_str(b: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0; b];
    let mut rng = rand::thread_rng();
    rng.fill_bytes(&mut buf);
    hex::encode(&buf)
}

pub async fn insert_ride_status(
    tx: &mut sqlx::MySqlConnection,
    ride_id: &str,
    status: &str,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)")
        .bind(Ulid::new().to_string())
        .bind(ride_id)
        .bind(status)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE rides SET latest_status = ?, updated_at = updated_at WHERE id = ?")
        .bind(status)
        .bind(ride_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

pub fn spawn_matching(state: AppState) {
    if !state.matching_enabled {
        return;
    }
    tokio::spawn(async move {
        if let Err(err) = crate::internal_handlers::run_matching(&state).await {
            tracing::debug!("matching: {err}");
        }
    });
}

pub fn is_deadlock(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(|d| d.code())
        .map(|c| c == "1213" || c == "40001")
        .unwrap_or(false)
}

pub fn calculate_distance(
    a_latitude: i32,
    a_longitude: i32,
    b_latitude: i32,
    b_longitude: i32,
) -> i32 {
    (a_latitude - b_latitude).abs() + (a_longitude - b_longitude).abs()
}

pub const INITIAL_FARE: i32 = 500;
pub const FARE_PER_DISTANCE: i32 = 100;

pub fn calculate_fare(
    pickup_latitude: i32,
    pickup_longitude: i32,
    dest_latitude: i32,
    dest_longitude: i32,
) -> i32 {
    let metered_fare = FARE_PER_DISTANCE
        * calculate_distance(
            pickup_latitude,
            pickup_longitude,
            dest_latitude,
            dest_longitude,
        );
    INITIAL_FARE + metered_fare
}

pub fn discounted_fare(
    pickup_latitude: i32,
    pickup_longitude: i32,
    dest_latitude: i32,
    dest_longitude: i32,
    discount: i32,
) -> i32 {
    let metered_fare = FARE_PER_DISTANCE
        * calculate_distance(
            pickup_latitude,
            pickup_longitude,
            dest_latitude,
            dest_longitude,
        );
    INITIAL_FARE + std::cmp::max(metered_fare - discount, 0)
}

pub const NOTIFICATION_RETRY_AFTER_MS: i32 = 30;

pub mod app_handlers;
pub mod chair_handlers;
pub mod http;
pub mod internal_handlers;
pub mod json;
pub mod models;
pub mod owner_handlers;
pub mod payment_gateway;
pub mod state;
