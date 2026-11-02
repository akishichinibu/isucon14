use isuride::AppState;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();

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

    let public_dir = std::env::var("ISUCON_PUBLIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("../public"));
    let static_files = Arc::new(isuride::http::load_static_files(&public_dir));

    let app_state = AppState {
        pool,
        matching_lock: Arc::new(tokio::sync::Mutex::new(())),
        world: std::sync::Arc::new(std::sync::RwLock::new(world)),
        matching_enabled,
        static_files,
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

    let plain = TcpListener::bind(&SocketAddr::from(([0, 0, 0, 0], 8080))).await?;
    let state_plain = app_state.clone();
    tokio::spawn(async move {
        if let Err(err) = isuride::http::serve(plain, state_plain).await {
            tracing::error!("plain 8080: {err}");
        }
    });

    let cert = std::env::var("ISUCON_TLS_CERT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/nginx/tls/_.xiv.isucon.net.crt"));
    let key = std::env::var("ISUCON_TLS_KEY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/etc/nginx/tls/_.xiv.isucon.net.key"));
    match isuride::http::load_tls_acceptor(&cert, &key) {
        Ok(acceptor) => {
            let tls = TcpListener::bind(&SocketAddr::from(([0, 0, 0, 0], 443))).await?;
            tracing::warn!("tls 443 ready");
            isuride::http::serve_tls(tls, acceptor, app_state).await?;
        }
        Err(err) => {
            tracing::warn!("tls disabled: {err}");
            std::future::pending::<()>().await;
        }
    }
    Ok(())
}
