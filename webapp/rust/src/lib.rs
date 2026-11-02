use axum::{http::StatusCode, response::Response};
use std::sync::Arc;
use ulid::Ulid;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::MySqlPool,
    pub matching_lock: Arc<tokio::sync::Mutex<()>>,
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
impl axum::response::IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Unauthorized(_) => StatusCode::UNAUTHORIZED,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::PaymentGateway(_) => StatusCode::BAD_GATEWAY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        #[derive(Debug, serde::Serialize)]
        struct ErrorBody {
            message: String,
        }
        let message = self.to_string();
        if status.is_server_error() {
            tracing::error!("{message}");
        }

        (status, axum::Json(ErrorBody { message })).into_response()
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct Coordinate {
    pub latitude: i32,
    pub longitude: i32,
}

pub fn secure_random_str(b: usize) -> String {
    use rand::RngCore as _;
    let mut buf = vec![0; b];
    let mut rng = rand::thread_rng();
    rng.fill_bytes(&mut buf);
    hex::encode(&buf)
}

pub async fn get_latest_ride_status<'e, E>(executor: E, ride_id: &str) -> sqlx::Result<String>
where
    E: 'e + sqlx::Executor<'e, Database = sqlx::MySql>,
{
    sqlx::query_scalar("SELECT latest_status FROM rides WHERE id = ?")
        .bind(ride_id)
        .fetch_one(executor)
        .await
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
    sqlx::query("UPDATE rides SET latest_status = ? WHERE id = ?")
        .bind(status)
        .bind(ride_id)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

pub fn spawn_matching(state: AppState) {
    tokio::spawn(async move {
        if let Err(err) = crate::internal_handlers::run_matching(&state).await {
            tracing::debug!("matching: {err}");
        }
    });
}

// Manhattan distance
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

pub const NOTIFICATION_RETRY_AFTER_MS: i32 = 100;

pub mod app_handlers;
pub mod chair_handlers;
pub mod internal_handlers;
pub mod middlewares;
pub mod models;
pub mod owner_handlers;
pub mod payment_gateway;
