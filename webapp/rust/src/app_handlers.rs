use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum_extra::extract::CookieJar;
use ulid::Ulid;

use crate::models::{Coupon, PaymentToken, Ride, User};
use crate::{
    discounted_fare, insert_ride_status, AppState, Coordinate, Error, NOTIFICATION_RETRY_AFTER_MS,
};

pub fn app_routes(app_state: AppState) -> axum::Router<AppState> {
    let routes = axum::Router::new().route("/api/app/users", axum::routing::post(app_post_users));

    let authed_routes = axum::Router::new()
        .route(
            "/api/app/payment-methods",
            axum::routing::post(app_post_payment_methods),
        )
        .route(
            "/api/app/rides",
            axum::routing::get(app_get_rides).post(app_post_rides),
        )
        .route(
            "/api/app/rides/estimated-fare",
            axum::routing::post(app_post_rides_estimated_fare),
        )
        .route(
            "/api/app/rides/:ride_id/evaluation",
            axum::routing::post(app_post_ride_evaluation),
        )
        .route(
            "/api/app/notification",
            axum::routing::get(app_get_notification),
        )
        .route(
            "/api/app/nearby-chairs",
            axum::routing::get(app_get_nearby_chairs),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::middlewares::app_auth_middleware,
        ));

    routes.merge(authed_routes)
}

#[derive(Debug, serde::Deserialize)]
struct AppPostUsersRequest {
    username: String,
    firstname: String,
    lastname: String,
    date_of_birth: String,
    invitation_code: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct AppPostUsersResponse {
    id: String,
    invitation_code: String,
}

async fn app_post_users(
    State(state): State<AppState>,
    jar: CookieJar,
    axum::Json(req): axum::Json<AppPostUsersRequest>,
) -> Result<(CookieJar, (StatusCode, axum::Json<AppPostUsersResponse>)), Error> {
    let user_id = Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let invitation_code = crate::secure_random_str(15);

    let mut last_err: Option<Error> = None;
    for _ in 0..8 {
        match app_post_users_once(&state.pool, &req, &user_id, &access_token, &invitation_code).await {
            Ok(()) => {
                last_err = None;
                break;
            }
            Err(Error::Sqlx(e)) if crate::is_deadlock(&e) => {
                last_err = Some(Error::Sqlx(e));
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(err) = last_err {
        return Err(err);
    }
    state.world.write().unwrap().remember_user(
        user_id.clone(),
        req.firstname.clone(),
        req.lastname.clone(),
    );

    let jar = jar.add(
        axum_extra::extract::cookie::Cookie::build(("app_session", access_token.clone())).path("/"),
    );
    return Ok((
        jar,
        (
            StatusCode::CREATED,
            axum::Json(AppPostUsersResponse {
                id: user_id,
                invitation_code,
            }),
        ),
    ));
}

async fn app_post_users_once(
    pool: &sqlx::MySqlPool,
    req: &AppPostUsersRequest,
    user_id: &str,
    access_token: &str,
    invitation_code: &str,
) -> Result<(), Error> {
    let mut tx = pool.begin().await?;

    sqlx::query("INSERT INTO users (id, username, firstname, lastname, date_of_birth, access_token, invitation_code) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(user_id)
        .bind(&req.username)
        .bind(&req.firstname)
        .bind(&req.lastname)
        .bind(&req.date_of_birth)
        .bind(access_token)
        .bind(invitation_code)
        .execute(&mut *tx)
        .await?;

    sqlx::query("INSERT INTO coupons (user_id, code, discount) VALUES (?, ?, ?)")
        .bind(user_id)
        .bind("CP_NEW2024")
        .bind(3000)
        .execute(&mut *tx)
        .await?;

    if let Some(req_invitation_code) = req.invitation_code.as_ref() {
        if !req_invitation_code.is_empty() {
            let coupons: Vec<Coupon> =
                sqlx::query_as("SELECT * FROM coupons WHERE code = ? FOR UPDATE")
                    .bind(format!("INV_{req_invitation_code}"))
                    .fetch_all(&mut *tx)
                    .await?;
            if coupons.len() >= 3 {
                return Err(Error::BadRequest("この招待コードは使用できません。"));
            }

            let Some(inviter): Option<User> =
                sqlx::query_as("SELECT id, username, firstname, lastname, date_of_birth, access_token, created_at, updated_at FROM users WHERE invitation_code = ?")
                    .bind(req_invitation_code)
                    .fetch_optional(&mut *tx)
                    .await?
            else {
                return Err(Error::BadRequest("この招待コードは使用できません。"));
            };

            sqlx::query("INSERT INTO coupons (user_id, code, discount) VALUES (?, ?, ?)")
                .bind(user_id)
                .bind(format!("INV_{req_invitation_code}"))
                .bind(1500)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO coupons (user_id, code, discount) VALUES (?, CONCAT(?, '_', FLOOR(UNIX_TIMESTAMP(NOW(3))*1000)), ?)")
                .bind(inviter.id)
                .bind(format!("RWD_{req_invitation_code}"))
                .bind(1000)
                .execute(&mut *tx)
                .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

#[derive(Debug, serde::Deserialize)]
struct AppPostPaymentMethodsRequest {
    token: String,
}

async fn app_post_payment_methods(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    axum::Json(req): axum::Json<AppPostPaymentMethodsRequest>,
) -> Result<StatusCode, Error> {
    sqlx::query("INSERT INTO payment_tokens (user_id, token) VALUES (?, ?)")
        .bind(user.id)
        .bind(req.token)
        .execute(&pool)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Serialize)]
struct GetAppRidesResponse {
    rides: Vec<GetAppRidesResponseItem>,
}

#[derive(Debug, serde::Serialize)]
struct GetAppRidesResponseItem {
    id: String,
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
    chair: GetAppRidesResponseItemChair,
    fare: i32,
    evaluation: i32,
    requested_at: i64,
    completed_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct GetAppRidesResponseItemChair {
    id: String,
    owner: String,
    name: String,
    model: String,
}

#[derive(Debug, sqlx::FromRow)]
struct CompletedRideItem {
    id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
    evaluation: i32,
    created_at: chrono::DateTime<chrono::Utc>,
    updated_at: chrono::DateTime<chrono::Utc>,
    chair_id: String,
    chair_name: String,
    chair_model: String,
    owner_name: String,
    discount: Option<i32>,
}

async fn app_get_rides(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
) -> Result<axum::Json<GetAppRidesResponse>, Error> {
    let rides: Vec<CompletedRideItem> = sqlx::query_as(
        r#"SELECT r.id,
                  r.pickup_latitude,
                  r.pickup_longitude,
                  r.destination_latitude,
                  r.destination_longitude,
                  r.evaluation,
                  r.created_at,
                  r.updated_at,
                  c.id AS chair_id,
                  c.name AS chair_name,
                  c.model AS chair_model,
                  o.name AS owner_name,
                  cp.discount AS discount
           FROM rides r
           INNER JOIN chairs c ON c.id = r.chair_id
           INNER JOIN owners o ON o.id = c.owner_id
           LEFT JOIN coupons cp ON cp.used_by = r.id
           WHERE r.user_id = ? AND r.latest_status = 'COMPLETED'
           ORDER BY r.created_at DESC"#,
    )
    .bind(&user.id)
    .fetch_all(&pool)
    .await?;

    let items = rides
        .into_iter()
        .map(|ride| GetAppRidesResponseItem {
            id: ride.id,
            pickup_coordinate: Coordinate {
                latitude: ride.pickup_latitude,
                longitude: ride.pickup_longitude,
            },
            destination_coordinate: Coordinate {
                latitude: ride.destination_latitude,
                longitude: ride.destination_longitude,
            },
            chair: GetAppRidesResponseItemChair {
                id: ride.chair_id,
                owner: ride.owner_name,
                name: ride.chair_name,
                model: ride.chair_model,
            },
            fare: discounted_fare(
                ride.pickup_latitude,
                ride.pickup_longitude,
                ride.destination_latitude,
                ride.destination_longitude,
                ride.discount.unwrap_or(0),
            ),
            evaluation: ride.evaluation,
            requested_at: ride.created_at.timestamp_millis(),
            completed_at: ride.updated_at.timestamp_millis(),
        })
        .collect();

    Ok(axum::Json(GetAppRidesResponse { rides: items }))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostRidesRequest {
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
}

#[derive(Debug, serde::Serialize)]
struct AppPostRidesResponse {
    ride_id: String,
    fare: i32,
}

async fn app_post_rides(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    axum::Json(req): axum::Json<AppPostRidesRequest>,
) -> Result<(StatusCode, axum::Json<AppPostRidesResponse>), Error> {
    let ride_id = Ulid::new().to_string();

    let mut tx = state.pool.begin().await?;

    let continuing: i64 = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM rides WHERE user_id = ? AND latest_status <> 'COMPLETED')",
    )
    .bind(&user.id)
    .fetch_one(&mut *tx)
    .await?;

    if continuing != 0 {
        return Err(Error::Conflict("ride already exists"));
    }

    sqlx::query("INSERT INTO rides (id, user_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, latest_status) VALUES (?, ?, ?, ?, ?, ?, 'MATCHING')")
        .bind(&ride_id)
        .bind(&user.id)
        .bind(req.pickup_coordinate.latitude)
        .bind(req.pickup_coordinate.longitude)
        .bind(req.destination_coordinate.latitude)
        .bind(req.destination_coordinate.longitude)
        .execute(&mut *tx)
        .await?;

    sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)")
        .bind(Ulid::new().to_string())
        .bind(&ride_id)
        .bind("MATCHING")
        .execute(&mut *tx)
        .await?;

    let ride_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rides WHERE user_id = ?")
        .bind(&user.id)
        .fetch_one(&mut *tx)
        .await?;

    if ride_count == 1 {
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND code = 'CP_NEW2024' AND used_by IS NULL FOR UPDATE")
            .bind(&user.id)
            .fetch_optional(&mut *tx)
            .await?;
        if coupon.is_some() {
            sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = 'CP_NEW2024'")
                .bind(&ride_id)
                .bind(&user.id)
                .execute(&mut *tx)
                .await?;
        } else {
            let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1 FOR UPDATE")
                .bind(&user.id)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(coupon) = coupon {
                sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = ?")
                    .bind(&ride_id)
                    .bind(&user.id)
                    .bind(coupon.code)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    } else {
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1 FOR UPDATE")
                .bind(&user.id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(coupon) = coupon {
            sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = ?")
                .bind(&ride_id)
                .bind(&user.id)
                .bind(coupon.code)
                .execute(&mut *tx)
                .await?;
        }
    }

    let ride: Ride = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(&ride_id)
        .fetch_one(&mut *tx)
        .await?;

    let fare = calculate_discounted_fare(
        &mut tx,
        &user.id,
        Some(&ride),
        req.pickup_coordinate.latitude,
        req.pickup_coordinate.longitude,
        req.destination_coordinate.latitude,
        req.destination_coordinate.longitude,
    )
    .await?;

    tx.commit().await?;

    {
        let mut world = state.world.write().unwrap();
        let now = chrono::Utc::now();
        world.add_ride(crate::state::RideMem {
            id: ride_id.clone(),
            user_id: user.id.clone(),
            chair_id: None,
            pickup_latitude: req.pickup_coordinate.latitude,
            pickup_longitude: req.pickup_coordinate.longitude,
            destination_latitude: req.destination_coordinate.latitude,
            destination_longitude: req.destination_coordinate.longitude,
            status: "MATCHING".to_string(),
            fare,
            created_at: now,
            updated_at: now,
        });
        world.push_unmatched(crate::state::UnmatchedRide {
            id: ride_id.clone(),
            pickup_latitude: req.pickup_coordinate.latitude,
            pickup_longitude: req.pickup_coordinate.longitude,
            destination_latitude: req.destination_coordinate.latitude,
            destination_longitude: req.destination_coordinate.longitude,
            created_at: now,
        });
    }

    crate::spawn_matching(state);

    Ok((
        StatusCode::ACCEPTED,
        axum::Json(AppPostRidesResponse { ride_id, fare }),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostRidesEstimatedFareRequest {
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
}

#[derive(Debug, serde::Serialize)]
struct AppPostRidesEstimatedFareResponse {
    fare: i32,
    discount: i32,
}

async fn app_post_rides_estimated_fare(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    axum::Json(req): axum::Json<AppPostRidesEstimatedFareRequest>,
) -> Result<axum::Json<AppPostRidesEstimatedFareResponse>, Error> {
    let mut tx = pool.begin().await?;

    let discounted = calculate_discounted_fare(
        &mut tx,
        &user.id,
        None,
        req.pickup_coordinate.latitude,
        req.pickup_coordinate.longitude,
        req.destination_coordinate.latitude,
        req.destination_coordinate.longitude,
    )
    .await?;

    tx.commit().await?;

    Ok(axum::Json(AppPostRidesEstimatedFareResponse {
        fare: discounted,
        discount: crate::calculate_fare(
            req.pickup_coordinate.latitude,
            req.pickup_coordinate.longitude,
            req.destination_coordinate.latitude,
            req.destination_coordinate.longitude,
        ) - discounted,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostRideEvaluationRequest {
    evaluation: i32,
}

#[derive(Debug, serde::Serialize)]
struct AppPostRideEvaluationResponse {
    fare: i32,
    completed_at: i64,
}

async fn app_post_ride_evaluation(
    State(state): State<AppState>,
    Path((ride_id,)): Path<(String,)>,
    axum::Json(req): axum::Json<AppPostRideEvaluationRequest>,
) -> Result<axum::Json<AppPostRideEvaluationResponse>, Error> {
    if req.evaluation < 1 || req.evaluation > 5 {
        return Err(Error::BadRequest("evaluation must be between 1 and 5"));
    }

    let mut tx = state.pool.begin().await?;

    let Some(ride): Option<Ride> = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(&ride_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(Error::NotFound("ride not found"));
    };

    if ride.latest_status != "ARRIVED" {
        return Err(Error::BadRequest("not arrived yet"));
    }

    let result = sqlx::query("UPDATE rides SET evaluation = ? WHERE id = ?")
        .bind(req.evaluation)
        .bind(&ride_id)
        .execute(&mut *tx)
        .await?;
    let count = result.rows_affected();
    if count == 0 {
        return Err(Error::NotFound("ride not found"));
    }

    insert_ride_status(&mut tx, &ride_id, "COMPLETED").await?;

    if let Some(chair_id) = ride.chair_id.as_ref() {
        sqlx::query(
            "UPDATE chairs SET total_rides_count = total_rides_count + 1, total_evaluation_sum = total_evaluation_sum + ? WHERE id = ?",
        )
        .bind(req.evaluation)
        .bind(chair_id)
        .execute(&mut *tx)
        .await?;
    }

    let Some(ride): Option<Ride> = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(&ride_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(Error::NotFound("ride not found"));
    };

    let Some(payment_token): Option<PaymentToken> =
        sqlx::query_as("SELECT * FROM payment_tokens WHERE user_id = ?")
            .bind(&ride.user_id)
            .fetch_optional(&mut *tx)
            .await?
    else {
        return Err(Error::BadRequest("payment token not registered"));
    };

    let fare = calculate_discounted_fare(
        &mut tx,
        &ride.user_id,
        Some(&ride),
        ride.pickup_latitude,
        ride.pickup_longitude,
        ride.destination_latitude,
        ride.destination_longitude,
    )
    .await?;

    let payment_gateway_url: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE name = 'payment_gateway_url'")
            .fetch_one(&mut *tx)
            .await?;

    async fn retrieve_rides_order_by_created_at_asc(
        tx: &mut sqlx::MySqlConnection,
        user_id: &str,
    ) -> Result<Vec<Ride>, Error> {
        sqlx::query_as("SELECT * FROM rides WHERE user_id = ? ORDER BY created_at ASC")
            .bind(user_id)
            .fetch_all(tx)
            .await
            .map_err(Error::Sqlx)
    }

    crate::payment_gateway::request_payment_gateway_post_payment(
        &payment_gateway_url,
        &payment_token.token,
        &crate::payment_gateway::PaymentGatewayPostPaymentRequest { amount: fare },
        &ride.id,
        &mut tx,
        &ride.user_id,
        retrieve_rides_order_by_created_at_asc,
    )
    .await?;

    tx.commit().await?;

    if let Some(chair_id) = ride.chair_id.as_ref() {
        state
            .world
            .write()
            .unwrap()
            .complete_ride(chair_id, req.evaluation);
    }
    crate::spawn_matching(state);

    Ok(axum::Json(AppPostRideEvaluationResponse {
        fare,
        completed_at: ride.updated_at.timestamp_millis(),
    }))
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponse {
    data: Option<AppGetNotificationResponseData>,
    retry_after_ms: i32,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponseData {
    ride_id: String,
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
    fare: i32,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chair: Option<AppGetNotificationResponseChair>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponseChair {
    id: String,
    name: String,
    model: String,
    stats: AppGetNotificationResponseChairStats,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponseChairStats {
    total_rides_count: i32,
    total_evaluation_avg: f64,
}

#[derive(Debug, sqlx::FromRow)]
struct ChairStatsRow {
    id: String,
    name: String,
    model: String,
    total_rides_count: i32,
    total_evaluation_sum: i32,
}

async fn app_get_notification(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
) -> Result<axum::Json<AppGetNotificationResponse>, Error> {
    let ev = {
        let mut world = state.world.write().unwrap();
        world
            .pop_app_event(&user.id)
            .or_else(|| world.peek_app_snapshot(&user.id))
    };
    let Some(ev) = ev else {
        return Ok(axum::Json(AppGetNotificationResponse {
            data: None,
            retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
        }));
    };

    let world = state.world.read().unwrap();
    let Some(ride) = world.rides.get(&ev.ride_id) else {
        return Ok(axum::Json(AppGetNotificationResponse {
            data: None,
            retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
        }));
    };
    let mut data = AppGetNotificationResponseData {
        ride_id: ride.id.clone(),
        pickup_coordinate: Coordinate {
            latitude: ride.pickup_latitude,
            longitude: ride.pickup_longitude,
        },
        destination_coordinate: Coordinate {
            latitude: ride.destination_latitude,
            longitude: ride.destination_longitude,
        },
        fare: ride.fare,
        status: ev.status,
        chair: None,
        created_at: ride.created_at.timestamp_millis(),
        updated_at: ride.updated_at.timestamp_millis(),
    };
    if let Some(chair_id) = ride.chair_id.as_ref() {
        if let Some(chair) = world.chairs.get(chair_id) {
            let avg = if chair.total_rides_count > 0 {
                chair.total_evaluation_sum as f64 / chair.total_rides_count as f64
            } else {
                0.0
            };
            data.chair = Some(AppGetNotificationResponseChair {
                id: chair.id.clone(),
                name: chair.name.clone(),
                model: chair.model.clone(),
                stats: AppGetNotificationResponseChairStats {
                    total_rides_count: chair.total_rides_count,
                    total_evaluation_avg: avg,
                },
            });
        }
    }
    drop(world);

    Ok(axum::Json(AppGetNotificationResponse {
        data: Some(data),
        retry_after_ms: NOTIFICATION_RETRY_AFTER_MS,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct AppGetNearbyChairsQuery {
    latitude: i32,
    longitude: i32,
    distance: Option<i32>,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNearbyChairsResponse {
    chairs: Vec<AppGetNearbyChairsResponseChair>,
    retrieved_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNearbyChairsResponseChair {
    id: String,
    name: String,
    model: String,
    current_coordinate: Coordinate,
}

#[derive(Debug, sqlx::FromRow)]
struct NearbyChairRow {
    id: String,
    name: String,
    model: String,
    latitude: i32,
    longitude: i32,
}

async fn app_get_nearby_chairs(
    State(state): State<AppState>,
    Query(query): Query<AppGetNearbyChairsQuery>,
) -> Result<axum::Json<AppGetNearbyChairsResponse>, Error> {
    let distance = query.distance.unwrap_or(50);
    let chairs = state
        .world
        .read()
        .unwrap()
        .nearby(query.latitude, query.longitude, distance);
    let retrieved_at = chrono::Utc::now();

    Ok(axum::Json(AppGetNearbyChairsResponse {
        chairs: chairs
            .into_iter()
            .map(|chair| AppGetNearbyChairsResponseChair {
                id: chair.id,
                name: chair.name,
                model: chair.model,
                current_coordinate: Coordinate {
                    latitude: chair.latitude.unwrap_or(0),
                    longitude: chair.longitude.unwrap_or(0),
                },
            })
            .collect(),
        retrieved_at: retrieved_at.timestamp_millis(),
    }))
}

async fn calculate_discounted_fare(
    tx: &mut sqlx::MySqlConnection,
    user_id: &str,
    ride: Option<&Ride>,
    mut pickup_latitude: i32,
    mut pickup_longitude: i32,
    mut dest_latitude: i32,
    mut dest_longitude: i32,
) -> sqlx::Result<i32> {
    let discount = if let Some(ride) = ride {
        dest_latitude = ride.destination_latitude;
        dest_longitude = ride.destination_longitude;
        pickup_latitude = ride.pickup_latitude;
        pickup_longitude = ride.pickup_longitude;

        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE used_by = ?")
            .bind(&ride.id)
            .fetch_optional(&mut *tx)
            .await?;
        coupon.map(|c| c.discount).unwrap_or(0)
    } else {
        let coupon: Option<Coupon> = sqlx::query_as(
            "SELECT * FROM coupons WHERE user_id = ? AND code = 'CP_NEW2024' AND used_by IS NULL",
        )
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(coupon) = coupon {
            coupon.discount
        } else {
            let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
            coupon.map(|c| c.discount).unwrap_or(0)
        }
    };

    Ok(discounted_fare(
        pickup_latitude,
        pickup_longitude,
        dest_latitude,
        dest_longitude,
        discount,
    ))
}
