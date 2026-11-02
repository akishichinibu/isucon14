//! HTTP/1.1 state machine + ISURIDE route trie. No framework.

use std::io::Write as _;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::json::error_body;
use crate::{AppState, Error};

const MAX_HEADER: usize = 16 * 1024;
const MAX_BODY: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

#[derive(Debug)]
pub enum Route {
    PostInitialize,
    PostAppUsers,
    PostAppPaymentMethods,
    GetAppRides,
    PostAppRides,
    PostAppEstimatedFare,
    PostAppEvaluation { ride_id: String },
    GetAppNotification,
    GetAppNearbyChairs,
    PostOwnerOwners,
    GetOwnerSales,
    GetOwnerChairs,
    PostChairChairs,
    PostChairActivity,
    PostChairCoordinate,
    GetChairNotification,
    PostChairRideStatus { ride_id: String },
    GetInternalMatching,
    Static { path: String },
}

pub struct HttpResp {
    pub status: u16,
    pub body: Vec<u8>,
    pub set_cookie: Option<(&'static str, String)>,
    pub content_type: Option<String>,
}

impl HttpResp {
    pub fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            set_cookie: None,
            content_type: Some("application/json".to_string()),
        }
    }
    pub fn empty(status: u16) -> Self {
        Self {
            status,
            body: Vec::new(),
            set_cookie: None,
            content_type: None,
        }
    }
    pub fn bytes(status: u16, content_type: String, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            set_cookie: None,
            content_type: Some(content_type),
        }
    }
    pub fn cookie(mut self, name: &'static str, value: String) -> Self {
        self.set_cookie = Some((name, value));
        self
    }
}

impl From<Error> for HttpResp {
    fn from(err: Error) -> Self {
        let status = match &err {
            Error::BadRequest(_) => 400,
            Error::Unauthorized(_) => 401,
            Error::NotFound(_) => 404,
            Error::Conflict(_) => 409,
            Error::PaymentGateway(_) => 502,
            _ => 500,
        };
        let message = err.to_string();
        if status >= 500 {
            tracing::error!("{message}");
        }
        HttpResp::json(status, error_body(&message))
    }
}

pub struct Parsed {
    pub method: Method,
    pub route: Route,
    pub query: Vec<(String, String)>,
    pub keepalive: bool,
    pub app_session: Option<String>,
    pub owner_session: Option<String>,
    pub chair_session: Option<String>,
    pub body: Vec<u8>,
}

pub fn query_i64(q: &[(String, String)], key: &str) -> Option<i64> {
    q.iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse().ok())
}

pub fn query_i32(q: &[(String, String)], key: &str) -> Option<i32> {
    q.iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse().ok())
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn match_route(method: Method, path: &[u8]) -> Option<Route> {
    match (method, path) {
        (Method::Post, b"/api/initialize") => Some(Route::PostInitialize),
        (Method::Post, b"/api/app/users") => Some(Route::PostAppUsers),
        (Method::Post, b"/api/app/payment-methods") => Some(Route::PostAppPaymentMethods),
        (Method::Get, b"/api/app/rides") => Some(Route::GetAppRides),
        (Method::Post, b"/api/app/rides") => Some(Route::PostAppRides),
        (Method::Post, b"/api/app/rides/estimated-fare") => Some(Route::PostAppEstimatedFare),
        (Method::Get, b"/api/app/notification") => Some(Route::GetAppNotification),
        (Method::Get, b"/api/app/nearby-chairs") => Some(Route::GetAppNearbyChairs),
        (Method::Post, b"/api/owner/owners") => Some(Route::PostOwnerOwners),
        (Method::Get, b"/api/owner/sales") => Some(Route::GetOwnerSales),
        (Method::Get, b"/api/owner/chairs") => Some(Route::GetOwnerChairs),
        (Method::Post, b"/api/chair/chairs") => Some(Route::PostChairChairs),
        (Method::Post, b"/api/chair/activity") => Some(Route::PostChairActivity),
        (Method::Post, b"/api/chair/coordinate") => Some(Route::PostChairCoordinate),
        (Method::Get, b"/api/chair/notification") => Some(Route::GetChairNotification),
        (Method::Get, b"/api/internal/matching") => Some(Route::GetInternalMatching),
        (Method::Post, p) if p.starts_with(b"/api/app/rides/") && p.ends_with(b"/evaluation") => {
            let start = b"/api/app/rides/".len();
            let end = p.len() - b"/evaluation".len();
            if end <= start {
                return None;
            }
            Some(Route::PostAppEvaluation {
                ride_id: String::from_utf8_lossy(&p[start..end]).into_owned(),
            })
        }
        (Method::Post, p) if p.starts_with(b"/api/chair/rides/") && p.ends_with(b"/status") => {
            let start = b"/api/chair/rides/".len();
            let end = p.len() - b"/status".len();
            if end <= start {
                return None;
            }
            Some(Route::PostChairRideStatus {
                ride_id: String::from_utf8_lossy(&p[start..end]).into_owned(),
            })
        }
        (Method::Get, p) if !p.starts_with(b"/api/") => Some(Route::Static {
            path: String::from_utf8_lossy(p).into_owned(),
        }),
        _ => None,
    }
}

fn parse_query(q: &[u8]) -> Vec<(String, String)> {
    if q.is_empty() {
        return Vec::new();
    }
    String::from_utf8_lossy(q)
        .split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

fn cookie_value(header: &str, name: &str) -> Option<String> {
    for part in header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn parse_headers(raw: &[u8]) -> Result<(Method, Route, Vec<(String, String)>, bool, Option<String>, Option<String>, Option<String>, usize), Error> {
    let text = std::str::from_utf8(raw).map_err(|_| Error::BadRequest("invalid header utf8"))?;
    let mut lines = text.split("\r\n");
    let req = lines.next().ok_or(Error::BadRequest("empty request"))?;
    let mut parts = req.splitn(3, ' ');
    let method = match parts.next() {
        Some("GET") => Method::Get,
        Some("POST") => Method::Post,
        _ => return Err(Error::BadRequest("method")),
    };
    let target = parts.next().ok_or(Error::BadRequest("path"))?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.as_bytes(), parse_query(q.as_bytes())),
        None => (target.as_bytes(), Vec::new()),
    };
    let route = match_route(method, path).ok_or(Error::NotFound("not found"))?;

    let mut content_length = 0usize;
    let mut keepalive = true;
    let mut cookie = String::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value.parse().unwrap_or(0);
        } else if name.eq_ignore_ascii_case("connection") {
            keepalive = !value.eq_ignore_ascii_case("close");
        } else if name.eq_ignore_ascii_case("cookie") {
            if !cookie.is_empty() {
                cookie.push(';');
            }
            cookie.push_str(value);
        }
    }
    let app_session = cookie_value(&cookie, "app_session");
    let owner_session = cookie_value(&cookie, "owner_session");
    let chair_session = cookie_value(&cookie, "chair_session");
    Ok((
        method,
        route,
        query,
        keepalive,
        app_session,
        owner_session,
        chair_session,
        content_length,
    ))
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

async fn write_resp<S: AsyncWrite + Unpin>(
    stream: &mut S,
    resp: HttpResp,
    keepalive: bool,
) -> std::io::Result<()> {
    let mut head = Vec::with_capacity(256 + resp.body.len());
    let _ = write!(
        &mut head,
        "HTTP/1.1 {} {}\r\n",
        resp.status,
        status_text(resp.status)
    );
    if let Some(ct) = resp.content_type {
        let _ = write!(&mut head, "Content-Type: {ct}\r\n");
    }
    let _ = write!(&mut head, "Content-Length: {}\r\n", resp.body.len());
    if let Some((name, value)) = resp.set_cookie {
        let _ = write!(&mut head, "Set-Cookie: {name}={value}; Path=/\r\n");
    }
    if keepalive {
        head.extend_from_slice(b"Connection: keep-alive\r\n");
    } else {
        head.extend_from_slice(b"Connection: close\r\n");
    }
    head.extend_from_slice(b"\r\n");
    stream.write_all(&head).await?;
    if resp.status != 204 && !resp.body.is_empty() {
        stream.write_all(&resp.body).await?;
    }
    stream.flush().await?;
    Ok(())
}

async fn dispatch(state: AppState, req: Parsed) -> HttpResp {
    let out = match req.route {
        Route::PostInitialize => crate::app_handlers::post_initialize(&state, &req.body).await,
        Route::PostAppUsers => crate::app_handlers::app_post_users(&state, &req.body).await,
        Route::PostAppPaymentMethods => {
            crate::app_handlers::app_post_payment_methods(&state, req.app_session.as_deref(), &req.body)
                .await
        }
        Route::GetAppRides => crate::app_handlers::app_get_rides(&state, req.app_session.as_deref()).await,
        Route::PostAppRides => {
            crate::app_handlers::app_post_rides(&state, req.app_session.as_deref(), &req.body).await
        }
        Route::PostAppEstimatedFare => {
            crate::app_handlers::app_post_estimated_fare(&state, req.app_session.as_deref(), &req.body)
                .await
        }
        Route::PostAppEvaluation { ride_id } => {
            crate::app_handlers::app_post_ride_evaluation(&state, req.app_session.as_deref(), &ride_id, &req.body)
                .await
        }
        Route::GetAppNotification => {
            crate::app_handlers::app_get_notification(&state, req.app_session.as_deref()).await
        }
        Route::GetAppNearbyChairs => {
            crate::app_handlers::app_get_nearby_chairs(&state, req.app_session.as_deref(), &req.query)
                .await
        }
        Route::PostOwnerOwners => crate::owner_handlers::owner_post_owners(&state, &req.body).await,
        Route::GetOwnerSales => {
            crate::owner_handlers::owner_get_sales(&state, req.owner_session.as_deref(), &req.query)
                .await
        }
        Route::GetOwnerChairs => {
            crate::owner_handlers::owner_get_chairs(&state, req.owner_session.as_deref())
                .await
        }
        Route::PostChairChairs => crate::chair_handlers::chair_post_chairs(&state, &req.body).await,
        Route::PostChairActivity => {
            crate::chair_handlers::chair_post_activity(&state, req.chair_session.as_deref(), &req.body)
                .await
        }
        Route::PostChairCoordinate => {
            crate::chair_handlers::chair_post_coordinate(&state, req.chair_session.as_deref(), &req.body)
                .await
        }
        Route::GetChairNotification => {
            crate::chair_handlers::chair_get_notification(&state, req.chair_session.as_deref())
                .await
        }
        Route::PostChairRideStatus { ride_id } => {
            crate::chair_handlers::chair_post_ride_status(
                &state,
                req.chair_session.as_deref(),
                &ride_id,
                &req.body,
            )
            .await
        }
        Route::GetInternalMatching => crate::internal_handlers::internal_get_matching(&state).await,
        Route::Static { path } => Ok(serve_static(&state, &path)),
    };
    match out {
        Ok(r) => r,
        Err(e) => HttpResp::from(e),
    }
}

fn content_type_for(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|s| s.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

pub fn load_static_files(root: &Path) -> std::collections::HashMap<String, (String, Vec<u8>)> {
    let mut out = std::collections::HashMap::new();
    fn walk(
        dir: &Path,
        root: &Path,
        out: &mut std::collections::HashMap<String, (String, Vec<u8>)>,
    ) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.is_dir() {
                walk(&path, root, out);
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let url = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            let ct = content_type_for(&url).to_string();
            out.insert(url, (ct, bytes));
        }
    }
    walk(root, root, &mut out);
    if let Some(index) = out.get("/index.html").cloned() {
        out.entry("/".to_string()).or_insert_with(|| index.clone());
        out.entry("/client".to_string())
            .or_insert_with(|| index.clone());
        out.entry("/owner".to_string()).or_insert(index);
    }
    out
}

fn serve_static(state: &AppState, path: &str) -> HttpResp {
    if path.contains("..") {
        return HttpResp::from(Error::NotFound("not found"));
    }
    if let Some((ct, body)) = state.static_files.get(path) {
        return HttpResp::bytes(200, ct.clone(), body.clone());
    }
    if let Some((ct, body)) = state.static_files.get("/index.html") {
        return HttpResp::bytes(200, ct.clone(), body.clone());
    }
    HttpResp::from(Error::NotFound("not found"))
}

async fn handle_conn<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, state: AppState) {
    let mut buf = vec![0u8; 8192];
    let mut filled = 0usize;
    loop {
        let header_end = loop {
            if let Some(pos) = find_header_end(&buf[..filled]) {
                break pos;
            }
            if filled >= MAX_HEADER {
                return;
            }
            if filled == buf.len() {
                buf.resize((buf.len() * 2).min(MAX_HEADER + MAX_BODY), 0);
            }
            match tokio::time::timeout(Duration::from_secs(65), stream.read(&mut buf[filled..]))
                .await
            {
                Ok(Ok(0)) | Err(_) => return,
                Ok(Ok(n)) => filled += n,
                Ok(Err(_)) => return,
            }
        };
        let parsed_headers = match parse_headers(&buf[..header_end]) {
            Ok(h) => h,
            Err(e) => {
                let _ = write_resp(&mut stream, HttpResp::from(e), false).await;
                return;
            }
        };
        let (
            method,
            route,
            query,
            keepalive,
            app_session,
            owner_session,
            chair_session,
            content_length,
        ) = parsed_headers;
        if content_length > MAX_BODY {
            return;
        }
        let body_start = header_end + 4;
        let need = body_start + content_length;
        while filled < need {
            if filled == buf.len() {
                buf.resize((buf.len() * 2).max(need).min(MAX_HEADER + MAX_BODY), 0);
            }
            match stream.read(&mut buf[filled..]).await {
                Ok(0) => return,
                Ok(n) => filled += n,
                Err(_) => return,
            }
        }
        let body = buf[body_start..need].to_vec();
        let leftover = filled - need;
        if leftover > 0 {
            buf.copy_within(need..filled, 0);
        }
        filled = leftover;

        let req = Parsed {
            method,
            route,
            query,
            keepalive,
            app_session,
            owner_session,
            chair_session,
            body,
        };
        let resp = dispatch(state.clone(), req).await;
        if write_resp(&mut stream, resp, keepalive).await.is_err() {
            return;
        }
        if !keepalive {
            return;
        }
    }
}

pub fn load_tls_acceptor(cert_path: &Path, key_path: &Path) -> anyhow::Result<tokio_rustls::TlsAcceptor> {
    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;
    let mut cert_reader = std::io::Cursor::new(cert_pem);
    let mut key_reader = std::io::Cursor::new(key_pem);
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut key_reader)?
        .ok_or_else(|| anyhow::anyhow!("no private key"))?;
    let cfg = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(cfg)))
}

pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                let state = state.clone();
                tokio::spawn(async move {
                    handle_conn(stream, state).await;
                });
            }
            Err(err) => {
                tracing::error!("accept: {err}");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}

pub async fn serve_tls(
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    state: AppState,
) -> std::io::Result<()> {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let _ = stream.set_nodelay(true);
                let state = state.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(stream).await else {
                        return;
                    };
                    handle_conn(tls, state).await;
                });
            }
            Err(err) => {
                tracing::error!("tls accept: {err}");
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
}
