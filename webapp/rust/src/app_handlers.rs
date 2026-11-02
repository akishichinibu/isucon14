use ulid::Ulid;

use crate::http::{query_i32, HttpResp};
use crate::json::Buf;
use crate::models::{Coupon, PaymentToken, Ride, User};
use crate::{
    discounted_fare, insert_ride_status, require_user, AppState, Error, NOTIFICATION_RETRY_AFTER_MS,
};

pub async fn post_initialize(state: &AppState, body: &[u8]) -> Result<HttpResp, Error> {
    let payment_server = crate::json::parse_named_string(body, "payment_server")?;
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
        .bind(&payment_server)
        .execute(&state.pool)
        .await?;

    let loaded = crate::state::load_world(&state.pool).await?;
    {
        let mut world = state.world.write().unwrap();
        *world = loaded;
        world.paused = false;
    }

    let mut b = Buf::new();
    b.obj_open();
    b.key("language");
    b.str("rust");
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn app_post_users(state: &AppState, body: &[u8]) -> Result<HttpResp, Error> {
    let req = crate::json::parse_user_reg(body)?;
    let user_id = Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let invitation_code = crate::secure_random_str(15);

    let mut last_err: Option<Error> = None;
    for _ in 0..8 {
        match app_post_users_once(&state.pool, &req, &user_id, &access_token, &invitation_code)
            .await
        {
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
        access_token.clone(),
    );

    let mut b = Buf::new();
    b.obj_open();
    b.key("id");
    b.str(&user_id);
    b.comma();
    b.key("invitation_code");
    b.str(&invitation_code);
    b.obj_close();
    Ok(HttpResp::json(201, b.into_bytes()).cookie("app_session", access_token))
}

async fn app_post_users_once(
    pool: &sqlx::MySqlPool,
    req: &crate::json::UserReg,
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

pub async fn app_post_payment_methods(
    state: &AppState,
    token: Option<&str>,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
    let pay = crate::json::parse_named_string(body, "token")?;
    sqlx::query("INSERT INTO payment_tokens (user_id, token) VALUES (?, ?)")
        .bind(&user_id)
        .bind(&pay)
        .execute(&state.pool)
        .await?;
    Ok(HttpResp::empty(204))
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

fn coord(b: &mut Buf, lat: i32, lng: i32) {
    b.obj_open();
    b.key("latitude");
    b.i32(lat);
    b.comma();
    b.key("longitude");
    b.i32(lng);
    b.obj_close();
}

pub async fn app_get_rides(state: &AppState, token: Option<&str>) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
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
    .bind(&user_id)
    .fetch_all(&state.pool)
    .await?;

    let mut b = Buf::new();
    b.obj_open();
    b.key("rides");
    b.arr_open();
    for (i, ride) in rides.iter().enumerate() {
        if i > 0 {
            b.comma();
        }
        b.obj_open();
        b.key("id");
        b.str(&ride.id);
        b.comma();
        b.key("pickup_coordinate");
        coord(&mut b, ride.pickup_latitude, ride.pickup_longitude);
        b.comma();
        b.key("destination_coordinate");
        coord(
            &mut b,
            ride.destination_latitude,
            ride.destination_longitude,
        );
        b.comma();
        b.key("chair");
        b.obj_open();
        b.key("id");
        b.str(&ride.chair_id);
        b.comma();
        b.key("owner");
        b.str(&ride.owner_name);
        b.comma();
        b.key("name");
        b.str(&ride.chair_name);
        b.comma();
        b.key("model");
        b.str(&ride.chair_model);
        b.obj_close();
        b.comma();
        b.key("fare");
        b.i32(discounted_fare(
            ride.pickup_latitude,
            ride.pickup_longitude,
            ride.destination_latitude,
            ride.destination_longitude,
            ride.discount.unwrap_or(0),
        ));
        b.comma();
        b.key("evaluation");
        b.i32(ride.evaluation);
        b.comma();
        b.key("requested_at");
        b.i64(ride.created_at.timestamp_millis());
        b.comma();
        b.key("completed_at");
        b.i64(ride.updated_at.timestamp_millis());
        b.obj_close();
    }
    b.arr_close();
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn app_post_rides(
    state: &AppState,
    token: Option<&str>,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
    let (plat, plng, dlat, dlng) = crate::json::parse_two_coordinates(body)?;
    let continuing = state
        .world
        .read()
        .unwrap()
        .rides
        .values()
        .any(|r| r.user_id == user_id && r.status != "COMPLETED");
    if continuing {
        return Err(Error::Conflict("ride already exists"));
    }

    let mut last_err: Option<Error> = None;
    let mut ride_id = String::new();
    let mut fare = 0;
    for _ in 0..8 {
        ride_id = Ulid::new().to_string();
        match app_post_rides_once(state, &user_id, plat, plng, dlat, dlng, &ride_id).await {
            Ok(f) => {
                fare = f;
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

    let mut b = Buf::new();
    b.obj_open();
    b.key("ride_id");
    b.str(&ride_id);
    b.comma();
    b.key("fare");
    b.i32(fare);
    b.obj_close();
    Ok(HttpResp::json(202, b.into_bytes()))
}

async fn app_post_rides_once(
    state: &AppState,
    user_id: &str,
    plat: i32,
    plng: i32,
    dlat: i32,
    dlng: i32,
    ride_id: &str,
) -> Result<i32, Error> {
    let mut tx = state.pool.begin().await?;

    sqlx::query("INSERT INTO rides (id, user_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, latest_status) VALUES (?, ?, ?, ?, ?, ?, 'MATCHING')")
        .bind(ride_id)
        .bind(user_id)
        .bind(plat)
        .bind(plng)
        .bind(dlat)
        .bind(dlng)
        .execute(&mut *tx)
        .await?;

    sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)")
        .bind(Ulid::new().to_string())
        .bind(ride_id)
        .bind("MATCHING")
        .execute(&mut *tx)
        .await?;

    let ride_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rides WHERE user_id = ?")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;

    if ride_count == 1 {
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND code = 'CP_NEW2024' AND used_by IS NULL FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await?;
        if coupon.is_some() {
            sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = 'CP_NEW2024'")
                .bind(ride_id)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
        } else {
            let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1 FOR UPDATE")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(coupon) = coupon {
                sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = ?")
                    .bind(ride_id)
                    .bind(user_id)
                    .bind(coupon.code)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    } else {
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1 FOR UPDATE")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(coupon) = coupon {
            sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = ?")
                .bind(ride_id)
                .bind(user_id)
                .bind(coupon.code)
                .execute(&mut *tx)
                .await?;
        }
    }

    let ride: Ride = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(ride_id)
        .fetch_one(&mut *tx)
        .await?;

    let fare = calculate_discounted_fare(
        &mut tx,
        user_id,
        Some(&ride),
        plat,
        plng,
        dlat,
        dlng,
    )
    .await?;

    tx.commit().await?;

    {
        let mut world = state.world.write().unwrap();
        let now = chrono::Utc::now();
        world.add_ride(crate::state::RideMem {
            id: ride_id.to_string(),
            user_id: user_id.to_string(),
            chair_id: None,
            pickup_latitude: plat,
            pickup_longitude: plng,
            destination_latitude: dlat,
            destination_longitude: dlng,
            status: "MATCHING".to_string(),
            fare,
            created_at: now,
            updated_at: now,
        });
        world.push_unmatched(crate::state::UnmatchedRide {
            id: ride_id.to_string(),
            pickup_latitude: plat,
            pickup_longitude: plng,
            destination_latitude: dlat,
            destination_longitude: dlng,
            created_at: now,
        });
    }

    crate::spawn_matching(state.clone());
    Ok(fare)
}

pub async fn app_post_estimated_fare(
    state: &AppState,
    token: Option<&str>,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
    let (plat, plng, dlat, dlng) = crate::json::parse_two_coordinates(body)?;
    let mut tx = state.pool.begin().await?;
    let discounted =
        calculate_discounted_fare(&mut tx, &user_id, None, plat, plng, dlat, dlng).await?;
    tx.commit().await?;

    let mut b = Buf::new();
    b.obj_open();
    b.key("fare");
    b.i32(discounted);
    b.comma();
    b.key("discount");
    b.i32(crate::calculate_fare(plat, plng, dlat, dlng) - discounted);
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn app_post_ride_evaluation(
    state: &AppState,
    token: Option<&str>,
    ride_id: &str,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let _user_id = require_user(state, token).await?;
    let evaluation = crate::json::parse_named_i32(body, "evaluation")?;
    if evaluation < 1 || evaluation > 5 {
        return Err(Error::BadRequest("evaluation must be between 1 and 5"));
    }

    let mut tx = state.pool.begin().await?;

    let Some(mut ride): Option<Ride> = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(ride_id)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Err(Error::NotFound("ride not found"));
    };

    let mem_chair = state
        .world
        .read()
        .unwrap()
        .rides
        .get(ride_id)
        .and_then(|r| r.chair_id.clone());
    if ride.chair_id.is_none() {
        if let Some(cid) = mem_chair.clone() {
            sqlx::query("UPDATE rides SET chair_id = ?, updated_at = updated_at WHERE id = ?")
                .bind(&cid)
                .bind(ride_id)
                .execute(&mut *tx)
                .await?;
            ride.chair_id = Some(cid);
        }
    }

    let arrived = state
        .world
        .read()
        .unwrap()
        .rides
        .get(ride_id)
        .map(|r| r.status == "ARRIVED")
        .unwrap_or(false)
        || ride.latest_status == "ARRIVED";
    if !arrived {
        return Err(Error::BadRequest("not arrived yet"));
    }

    let result = sqlx::query("UPDATE rides SET evaluation = ? WHERE id = ?")
        .bind(evaluation)
        .bind(ride_id)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() == 0 {
        return Err(Error::NotFound("ride not found"));
    }

    insert_ride_status(&mut tx, ride_id, "COMPLETED").await?;

    if let Some(chair_id) = ride.chair_id.as_ref() {
        sqlx::query(
            "UPDATE chairs SET total_rides_count = total_rides_count + 1, total_evaluation_sum = total_evaluation_sum + ? WHERE id = ?",
        )
        .bind(evaluation)
        .bind(chair_id)
        .execute(&mut *tx)
        .await?;
    }

    let Some(ride): Option<Ride> = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(ride_id)
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

    {
        let mut world = state.world.write().unwrap();
        let mem_chair = world
            .rides
            .get(ride_id)
            .and_then(|r| r.chair_id.clone());
        let chair_id = mem_chair.or(ride.chair_id.clone());
        world.complete_ride(ride_id, chair_id.as_deref(), evaluation, ride.updated_at);
    }
    crate::spawn_matching(state.clone());

    let mut b = Buf::new();
    b.obj_open();
    b.key("fare");
    b.i32(fare);
    b.comma();
    b.key("completed_at");
    b.i64(ride.updated_at.timestamp_millis());
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn app_get_notification(
    state: &AppState,
    token: Option<&str>,
) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
    let ev = {
        let world = state.world.read().unwrap();
        world
            .pop_app_event(&user_id)
            .or_else(|| world.peek_app_snapshot(&user_id))
    };
    let mut b = Buf::new();
    b.obj_open();
    b.key("data");
    let Some(ev) = ev else {
        b.null();
        b.comma();
        b.key("retry_after_ms");
        b.i32(NOTIFICATION_RETRY_AFTER_MS);
        b.obj_close();
        return Ok(HttpResp::json(200, b.into_bytes()));
    };

    let world = state.world.read().unwrap();
    let Some(ride) = world.rides.get(&ev.ride_id) else {
        drop(world);
        let mut b = Buf::new();
        b.obj_open();
        b.key("data");
        b.null();
        b.comma();
        b.key("retry_after_ms");
        b.i32(NOTIFICATION_RETRY_AFTER_MS);
        b.obj_close();
        return Ok(HttpResp::json(200, b.into_bytes()));
    };
    b.obj_open();
    b.key("ride_id");
    b.str(&ride.id);
    b.comma();
    b.key("pickup_coordinate");
    coord(&mut b, ride.pickup_latitude, ride.pickup_longitude);
    b.comma();
    b.key("destination_coordinate");
    coord(
        &mut b,
        ride.destination_latitude,
        ride.destination_longitude,
    );
    b.comma();
    b.key("fare");
    b.i32(ride.fare);
    b.comma();
    b.key("status");
    b.str(&ev.status);
    if let Some(chair_id) = ride.chair_id.as_ref() {
        if let Some(chair) = world.chairs.get(chair_id) {
            let avg = if chair.total_rides_count > 0 {
                chair.total_evaluation_sum as f64 / chair.total_rides_count as f64
            } else {
                0.0
            };
            b.comma();
            b.key("chair");
            b.obj_open();
            b.key("id");
            b.str(&chair.id);
            b.comma();
            b.key("name");
            b.str(&chair.name);
            b.comma();
            b.key("model");
            b.str(&chair.model);
            b.comma();
            b.key("stats");
            b.obj_open();
            b.key("total_rides_count");
            b.i32(chair.total_rides_count);
            b.comma();
            b.key("total_evaluation_avg");
            b.f64(avg);
            b.obj_close();
            b.obj_close();
        }
    }
    b.comma();
    b.key("created_at");
    b.i64(ride.created_at.timestamp_millis());
    b.comma();
    b.key("updated_at");
    b.i64(ride.updated_at.timestamp_millis());
    b.obj_close();
    drop(world);
    b.comma();
    b.key("retry_after_ms");
    b.i32(NOTIFICATION_RETRY_AFTER_MS);
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn app_get_nearby_chairs(
    state: &AppState,
    token: Option<&str>,
    query: &[(String, String)],
) -> Result<HttpResp, Error> {
    let _user_id = require_user(state, token).await?;
    let latitude = query_i32(query, "latitude").ok_or(Error::BadRequest("latitude required"))?;
    let longitude = query_i32(query, "longitude").ok_or(Error::BadRequest("longitude required"))?;
    let distance = query_i32(query, "distance").unwrap_or(50);
    let chairs = state
        .world
        .read()
        .unwrap()
        .nearby(latitude, longitude, distance);
    let retrieved_at = chrono::Utc::now();

    let mut b = Buf::new();
    b.obj_open();
    b.key("chairs");
    b.arr_open();
    for (i, chair) in chairs.iter().enumerate() {
        if i > 0 {
            b.comma();
        }
        b.obj_open();
        b.key("id");
        b.str(&chair.id);
        b.comma();
        b.key("name");
        b.str(&chair.name);
        b.comma();
        b.key("model");
        b.str(&chair.model);
        b.comma();
        b.key("current_coordinate");
        coord(
            &mut b,
            chair.latitude.unwrap_or(0),
            chair.longitude.unwrap_or(0),
        );
        b.obj_close();
    }
    b.arr_close();
    b.comma();
    b.key("retrieved_at");
    b.i64(retrieved_at.timestamp_millis());
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
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
