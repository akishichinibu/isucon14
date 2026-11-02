use axum::extract::State;
use isuride::{AppState, Error};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let host = std::env::var("ISUCON_DB_HOST").unwrap_or_else(|_| "127.0.0.1".to_owned());
    let port = std::env::var("ISUCON_DB_PORT")
        .map(|port_str| {
            port_str.parse().expect(
                "failed to convert DB port number from ISUCON_DB_PORT environment variable into u16",
            )
        })
        .unwrap_or(3306);
    let user = std::env::var("ISUCON_DB_USER").unwrap_or_else(|_| "isucon".to_owned());
    let password = std::env::var("ISUCON_DB_PASSWORD").unwrap_or_else(|_| "isucon".to_owned());
    let dbname = std::env::var("ISUCON_DB_NAME").unwrap_or_else(|_| "isuride".to_owned());

    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(64)
        .min_connections(8)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(
            sqlx::mysql::MySqlConnectOptions::default()
                .host(&host)
                .port(port)
                .username(&user)
                .password(&password)
                .database(&dbname),
        )
        .await?;

    let matching_enabled = std::env::var("ISUCON_ENABLE_MATCHING")
        .map(|v| {
            let v = v.trim().trim_matches('"').to_ascii_lowercase();
            v != "0" && v != "false" && v != "off"
        })
        .unwrap_or(true);

    let world = isuride::state::load_world(&pool)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!("world load on boot failed (empty until initialize): {err}");
            isuride::state::World::default()
        });

    let app_state = AppState {
        pool,
        matching_lock: Arc::new(tokio::sync::Mutex::new(())),
        world: std::sync::Arc::new(std::sync::RwLock::new(world)),
        matching_enabled,
    };

    if matching_enabled {
        let state = app_state.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(err) = isuride::internal_handlers::try_run_matching(&state).await {
                    tracing::debug!("matching: {err}");
                }
            }
        });
    }

    let app = axum::Router::new()
        .route("/api/initialize", axum::routing::post(post_initialize))
        .merge(isuride::app_handlers::app_routes(app_state.clone()))
        .merge(isuride::owner_handlers::owner_routes(app_state.clone()))
        .merge(isuride::chair_handlers::chair_routes(app_state.clone()))
        .merge(isuride::internal_handlers::internal_routes())
        .with_state(app_state);

    let tcp_listener =
        if let Some(std_listener) = listenfd::ListenFd::from_env().take_tcp_listener(0)? {
            TcpListener::from_std(std_listener)?
        } else {
            TcpListener::bind(&SocketAddr::from(([0, 0, 0, 0], 8080))).await?
        };
    axum::serve(tcp_listener, app).await?;

    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct PostInitializeRequest {
    payment_server: String,
}

#[derive(Debug, serde::Serialize)]
struct PostInitializeResponse {
    language: &'static str,
}

async fn post_initialize(
    State(state): State<AppState>,
    axum::Json(req): axum::Json<PostInitializeRequest>,
) -> Result<axum::Json<PostInitializeResponse>, Error> {
    {
        let mut world = state.world.write().unwrap();
        world.paused = true;
        world.chairs.clear();
        world.unmatched.clear();
    }

    let output = tokio::process::Command::new("../sql/init.sh")
        .output()
        .await?;
    if !output.status.success() {
        state.world.write().unwrap().paused = false;
        return Err(Error::Initialize {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    sqlx::query("UPDATE settings SET value = ? WHERE name = 'payment_gateway_url'")
        .bind(req.payment_server)
        .execute(&state.pool)
        .await?;

    let loaded = isuride::state::load_world(&state.pool).await?;
    {
        let mut world = state.world.write().unwrap();
        *world = loaded;
        world.paused = false;
    }

    Ok(axum::Json(PostInitializeResponse { language: "rust" }))
}
