use isuride::AppState;
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

    let tcp_listener =
        if let Some(std_listener) = listenfd::ListenFd::from_env().take_tcp_listener(0)? {
            TcpListener::from_std(std_listener)?
        } else {
            TcpListener::bind(&SocketAddr::from(([0, 0, 0, 0], 8080))).await?
        };
    isuride::http::serve(tcp_listener, app_state).await?;
    Ok(())
}
