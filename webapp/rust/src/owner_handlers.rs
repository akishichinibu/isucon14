use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum_extra::extract::cookie::Cookie;
use axum_extra::extract::CookieJar;
use chrono::{DateTime, NaiveDate, Utc};

use crate::models::Owner;
use crate::{AppState, Error};

pub fn owner_routes(app_state: AppState) -> axum::Router<AppState> {
    let routes =
        axum::Router::new().route("/api/owner/owners", axum::routing::post(owner_post_owners));

    let authed_routes = axum::Router::new()
        .route("/api/owner/sales", axum::routing::get(owner_get_sales))
        .route("/api/owner/chairs", axum::routing::get(owner_get_chairs))
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::middlewares::owner_auth_middleware,
        ));

    routes.merge(authed_routes)
}

#[derive(Debug, serde::Deserialize)]
struct OwnerPostOwnersRequest {
    name: String,
}

#[derive(Debug, serde::Serialize)]
struct OwnerPostOwnersResponse {
    id: String,
    chair_register_token: String,
}

async fn owner_post_owners(
    State(AppState { pool, .. }): State<AppState>,
    jar: CookieJar,
    axum::Json(req): axum::Json<OwnerPostOwnersRequest>,
) -> Result<(CookieJar, (StatusCode, axum::Json<OwnerPostOwnersResponse>)), Error> {
    let owner_id = ulid::Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let chair_register_token = crate::secure_random_str(32);

    sqlx::query(
        "INSERT INTO owners (id, name, access_token, chair_register_token) VALUES (?, ?, ?, ?)",
    )
    .bind(&owner_id)
    .bind(req.name)
    .bind(&access_token)
    .bind(&chair_register_token)
    .execute(&pool)
    .await?;

    let jar = jar.add(Cookie::build(("owner_session", access_token)).path("/"));

    Ok((
        jar,
        (
            StatusCode::CREATED,
            axum::Json(OwnerPostOwnersResponse {
                id: owner_id,
                chair_register_token,
            }),
        ),
    ))
}

#[derive(Debug, serde::Serialize)]
struct ChairSales {
    id: String,
    name: String,
    sales: i32,
}

#[derive(Debug, serde::Serialize)]
struct ModelSales {
    model: String,
    sales: i32,
}

#[derive(Debug, serde::Serialize)]
struct OwnerGetSalesResponse {
    total_sales: i32,
    chairs: Vec<ChairSales>,
    models: Vec<ModelSales>,
}

#[derive(Debug, serde::Deserialize)]
struct GetOwnerSalesQuery {
    since: Option<i64>,
    until: Option<i64>,
}

fn sales_until(until_millis: Option<i64>) -> DateTime<Utc> {
    if let Some(until) = until_millis.and_then(DateTime::from_timestamp_millis) {
        // Official impl: `updated_at BETWEEN ? AND ? + INTERVAL 999 MICROSECOND`
        // because completed_at is unix millis (microseconds truncated).
        return until + chrono::Duration::microseconds(999);
    }
    NaiveDate::from_ymd_opt(9999, 12, 31)
        .unwrap()
        .and_hms_opt(23, 59, 59)
        .unwrap()
        .and_utc()
}

async fn owner_get_sales(
    State(state): State<AppState>,
    axum::Extension(owner): axum::Extension<Owner>,
    Query(query): Query<GetOwnerSalesQuery>,
) -> Result<axum::Json<OwnerGetSalesResponse>, Error> {
    let since = query
        .since
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(|| DateTime::from_timestamp_millis(0).unwrap());
    let until = sales_until(query.until);

    let (total, chair_sales, by_model) = state
        .world
        .read()
        .unwrap()
        .owner_sales(&owner.id, since, until);

    Ok(axum::Json(OwnerGetSalesResponse {
        total_sales: total,
        chairs: chair_sales
            .into_iter()
            .map(|(chair, sales)| ChairSales {
                id: chair.id,
                name: chair.name,
                sales,
            })
            .collect(),
        models: by_model
            .into_iter()
            .map(|(model, sales)| ModelSales { model, sales })
            .collect(),
    }))
}

#[derive(Debug, serde::Serialize)]
struct OwnerGetChairResponse {
    chairs: Vec<OwnerGetChairResponseChair>,
}

#[derive(Debug, serde::Serialize)]
struct OwnerGetChairResponseChair {
    id: String,
    name: String,
    model: String,
    active: bool,
    registered_at: i64,
    total_distance: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_distance_updated_at: Option<i64>,
}

async fn owner_get_chairs(
    State(state): State<AppState>,
    axum::Extension(owner): axum::Extension<Owner>,
) -> Result<axum::Json<OwnerGetChairResponse>, Error> {
    let chairs = state.world.read().unwrap().owner_chairs(&owner.id);
    Ok(axum::Json(OwnerGetChairResponse {
        chairs: chairs
            .into_iter()
            .map(|chair| OwnerGetChairResponseChair {
                id: chair.id,
                name: chair.name,
                model: chair.model,
                active: chair.is_active,
                registered_at: chair.created_at.timestamp_millis(),
                total_distance: chair.total_distance as i64,
                total_distance_updated_at: chair
                    .total_distance_updated_at
                    .map(|t| t.timestamp_millis()),
            })
            .collect(),
    }))
}
