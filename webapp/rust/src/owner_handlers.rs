use std::collections::HashMap;

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

#[derive(Debug, sqlx::FromRow)]
struct OwnerChairRow {
    id: String,
    name: String,
    model: String,
}

#[derive(Debug, sqlx::FromRow)]
struct CompletedRideRow {
    chair_id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
}

async fn owner_get_sales(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(owner): axum::Extension<Owner>,
    Query(query): Query<GetOwnerSalesQuery>,
) -> Result<axum::Json<OwnerGetSalesResponse>, Error> {
    let since = if let Some(since) = query.since {
        DateTime::from_timestamp_millis(since).unwrap()
    } else {
        DateTime::from_timestamp_millis(0).unwrap()
    };
    let until = if let Some(until) = query.until {
        DateTime::from_timestamp_millis(until).unwrap()
    } else {
        DateTime::from_naive_utc_and_offset(
            NaiveDate::from_ymd_opt(9999, 12, 31)
                .unwrap()
                .and_hms_opt(23, 59, 59)
                .unwrap(),
            Utc,
        )
    };

    let chairs: Vec<OwnerChairRow> =
        sqlx::query_as("SELECT id, name, model FROM chairs WHERE owner_id = ?")
            .bind(&owner.id)
            .fetch_all(&pool)
            .await?;

    let rides: Vec<CompletedRideRow> = sqlx::query_as(
        r#"SELECT r.chair_id, r.pickup_latitude, r.pickup_longitude, r.destination_latitude, r.destination_longitude
           FROM rides r
           INNER JOIN chairs c ON c.id = r.chair_id
           WHERE c.owner_id = ?
             AND r.latest_status = 'COMPLETED'
             AND r.updated_at BETWEEN ? AND ? + INTERVAL 999 MICROSECOND"#,
    )
    .bind(&owner.id)
    .bind(since)
    .bind(until)
    .fetch_all(&pool)
    .await?;

    let mut sales_by_chair: HashMap<String, i32> = HashMap::new();
    for ride in rides {
        *sales_by_chair.entry(ride.chair_id).or_insert(0) += crate::calculate_fare(
            ride.pickup_latitude,
            ride.pickup_longitude,
            ride.destination_latitude,
            ride.destination_longitude,
        );
    }

    let mut res = OwnerGetSalesResponse {
        total_sales: 0,
        chairs: Vec::with_capacity(chairs.len()),
        models: Vec::new(),
    };
    let mut model_sales_by_model: HashMap<String, i32> = HashMap::new();

    for chair in chairs {
        let sales = sales_by_chair.get(&chair.id).copied().unwrap_or(0);
        res.total_sales += sales;
        res.chairs.push(ChairSales {
            id: chair.id,
            name: chair.name,
            sales,
        });
        *model_sales_by_model.entry(chair.model).or_insert(0) += sales;
    }

    for (model, sales) in model_sales_by_model {
        res.models.push(ModelSales { model, sales });
    }

    Ok(axum::Json(res))
}

#[derive(Debug, sqlx::FromRow)]
struct ChairWithDetail {
    id: String,
    name: String,
    model: String,
    is_active: bool,
    created_at: DateTime<Utc>,
    total_distance: i64,
    total_distance_updated_at: Option<DateTime<Utc>>,
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
