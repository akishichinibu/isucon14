use ulid::Ulid;

use crate::http::{query_i32, HttpResp};
use crate::json::Buf;
use crate::models::{Coupon, User};
use crate::{require_user, AppState, Error, NOTIFICATION_RETRY_AFTER_MS};

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
        world.payment_gateway_url = payment_server;
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

    {
        let mut world = state.world.write().unwrap();
        if let Some(inv) = req.invitation_code.as_ref() {
            if !inv.is_empty() {
                if !world.invitation_by_code.contains_key(inv) {
                    return Err(Error::BadRequest("この招待コードは使用できません。"));
                }
                let inv_code = format!("INV_{inv}");
                let used = world
                    .coupons
                    .values()
                    .flatten()
                    .filter(|c| c.code == inv_code)
                    .count();
                if used >= 3 {
                    return Err(Error::BadRequest("この招待コードは使用できません。"));
                }
            }
        }
        world.remember_user(
            user_id.clone(),
            req.firstname.clone(),
            req.lastname.clone(),
            access_token.clone(),
            invitation_code.clone(),
        );
        let now = chrono::Utc::now();
        world.add_coupon(
            user_id.clone(),
            crate::state::CouponMem {
                code: "CP_NEW2024".to_string(),
                discount: 3000,
                used_by: None,
                created_at: now,
            },
        );
        if let Some(inv) = req.invitation_code.as_ref() {
            if !inv.is_empty() {
                world.add_coupon(
                    user_id.clone(),
                    crate::state::CouponMem {
                        code: format!("INV_{inv}"),
                        discount: 1500,
                        used_by: None,
                        created_at: now,
                    },
                );
                if let Some(inviter) = world.invitation_by_code.get(inv).cloned() {
                    world.add_coupon(
                        inviter,
                        crate::state::CouponMem {
                            code: format!("RWD_{inv}_{}", now.timestamp_millis()),
                            discount: 1000,
                            used_by: None,
                            created_at: now,
                        },
                    );
                }
            }
        }
    }

    let pool = state.pool.clone();
    let req_sql = (
        req.username.clone(),
        req.firstname.clone(),
        req.lastname.clone(),
        req.date_of_birth.clone(),
        req.invitation_code.clone(),
    );
    let uid = user_id.clone();
    let tok = access_token.clone();
    let inv_own = invitation_code.clone();
    tokio::spawn(async move {
        let _ = app_post_users_once(
            &pool,
            &crate::json::UserReg {
                username: req_sql.0,
                firstname: req_sql.1,
                lastname: req_sql.2,
                date_of_birth: req_sql.3,
                invitation_code: req_sql.4,
            },
            &uid,
            &tok,
            &inv_own,
        )
        .await;
    });

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
    state
        .world
        .write()
        .unwrap()
        .payment_tokens
        .insert(user_id.clone(), pay.clone());
    let pool = state.pool.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("INSERT INTO payment_tokens (user_id, token) VALUES (?, ?)")
            .bind(&user_id)
            .bind(&pay)
            .execute(&pool)
            .await;
    });
    Ok(HttpResp::empty(204))
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

struct RideHist {
    id: String,
    plat: i32,
    plng: i32,
    dlat: i32,
    dlng: i32,
    chair_id: String,
    chair_name: String,
    chair_model: String,
    owner_name: String,
    fare: i32,
    evaluation: i32,
    requested_at: i64,
    completed_at: i64,
}

pub async fn app_get_rides(state: &AppState, token: Option<&str>) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
    let world = state.world.read().unwrap();
    let mut rides: Vec<RideHist> = world
        .rides
        .values()
        .filter(|r| r.user_id == user_id && r.status == "COMPLETED")
        .filter_map(|r| {
            let cid = r.chair_id.as_ref()?;
            let chair = world.chairs.get(cid)?;
            let owner = world.owners.get(&chair.owner_id);
            Some(RideHist {
                id: r.id.clone(),
                plat: r.pickup_latitude,
                plng: r.pickup_longitude,
                dlat: r.destination_latitude,
                dlng: r.destination_longitude,
                chair_id: chair.id.clone(),
                chair_name: chair.name.clone(),
                chair_model: chair.model.clone(),
                owner_name: owner.map(|o| o.name.clone()).unwrap_or_default(),
                fare: r.fare,
                evaluation: r.evaluation,
                requested_at: r.created_at.timestamp_millis(),
                completed_at: r.updated_at.timestamp_millis(),
            })
        })
        .collect();
    rides.sort_by_key(|r| std::cmp::Reverse(r.requested_at));
    drop(world);

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
        coord(&mut b, ride.plat, ride.plng);
        b.comma();
        b.key("destination_coordinate");
        coord(&mut b, ride.dlat, ride.dlng);
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
        b.i32(ride.fare);
        b.comma();
        b.key("evaluation");
        b.i32(ride.evaluation);
        b.comma();
        b.key("requested_at");
        b.i64(ride.requested_at);
        b.comma();
        b.key("completed_at");
        b.i64(ride.completed_at);
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
    let ride_id = Ulid::new().to_string();
    let fare = {
        let mut world = state.world.write().unwrap();
        if world
            .rides
            .values()
            .any(|r| r.user_id == user_id && r.status != "COMPLETED")
        {
            return Err(Error::Conflict("ride already exists"));
        }
        let discount = world.consume_coupon(&user_id, &ride_id);
        let fare = crate::discounted_fare(plat, plng, dlat, dlng, discount);
        let now = chrono::Utc::now();
        world.add_ride(crate::state::RideMem {
            id: ride_id.clone(),
            user_id: user_id.clone(),
            chair_id: None,
            pickup_latitude: plat,
            pickup_longitude: plng,
            destination_latitude: dlat,
            destination_longitude: dlng,
            status: "MATCHING".to_string(),
            fare,
            evaluation: 0,
            discount,
            created_at: now,
            updated_at: now,
        });
        world.push_unmatched(crate::state::UnmatchedRide {
            id: ride_id.clone(),
            pickup_latitude: plat,
            pickup_longitude: plng,
            destination_latitude: dlat,
            destination_longitude: dlng,
            created_at: now,
        });
        fare
    };
    crate::spawn_matching(state.clone());
    let pool = state.pool.clone();
    let rid = ride_id.clone();
    let uid = user_id.clone();
    tokio::spawn(async move {
        let _ = sqlx::query("INSERT INTO rides (id, user_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude, latest_status) VALUES (?, ?, ?, ?, ?, ?, 'MATCHING')")
            .bind(&rid)
            .bind(&uid)
            .bind(plat)
            .bind(plng)
            .bind(dlat)
            .bind(dlng)
            .execute(&pool)
            .await;
        let _ = sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)")
            .bind(Ulid::new().to_string())
            .bind(&rid)
            .bind("MATCHING")
            .execute(&pool)
            .await;
        let _ = sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1")
            .bind(&rid)
            .bind(&uid)
            .execute(&pool)
            .await;
    });

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

pub async fn app_post_estimated_fare(
    state: &AppState,
    token: Option<&str>,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let user_id = require_user(state, token).await?;
    let (plat, plng, dlat, dlng) = crate::json::parse_two_coordinates(body)?;
    let discount = state.world.read().unwrap().peek_discount(&user_id);
    let discounted = crate::discounted_fare(plat, plng, dlat, dlng, discount);
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

    let (arrived, fare, pay_token, pay_url, user_id, chair_id, ride_count) = {
        let world = state.world.read().unwrap();
        let ride = world
            .rides
            .get(ride_id)
            .ok_or(Error::NotFound("ride not found"))?;
        if ride.status != "ARRIVED" {
            return Err(Error::BadRequest("not arrived yet"));
        }
        let token = world
            .payment_tokens
            .get(&ride.user_id)
            .cloned()
            .ok_or(Error::BadRequest("payment token not registered"))?;
        let url = world.payment_gateway_url.clone();
        if url.is_empty() {
            return Err(Error::BadRequest("payment token not registered"));
        }
        let n = world
            .rides
            .values()
            .filter(|r| r.user_id == ride.user_id)
            .count();
        (
            true,
            ride.fare,
            token,
            url,
            ride.user_id.clone(),
            ride.chair_id.clone(),
            n,
        )
    };
    let _ = arrived;

    crate::payment_gateway::request_payment_gateway_post_payment(
        &pay_url,
        &pay_token,
        fare,
        ride_id,
        ride_count,
    )
    .await?;

    let completed_at = chrono::Utc::now();
    {
        let mut world = state.world.write().unwrap();
        world.complete_ride(ride_id, chair_id.as_deref(), evaluation, completed_at);
    }
    crate::spawn_matching(state.clone());

    let pool = state.pool.clone();
    let rid = ride_id.to_string();
    let cid = chair_id.clone();
    tokio::spawn(async move {
        let _ = sqlx::query(
            "UPDATE rides SET evaluation = ?, latest_status = 'COMPLETED', chair_id = COALESCE(?, chair_id), updated_at = updated_at WHERE id = ?",
        )
        .bind(evaluation)
        .bind(&cid)
        .bind(&rid)
        .execute(&pool)
        .await;
        let _ = sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, 'COMPLETED')")
            .bind(ulid::Ulid::new().to_string())
            .bind(&rid)
            .execute(&pool)
            .await;
        if let Some(chair_id) = cid {
            let _ = sqlx::query(
                "UPDATE chairs SET total_rides_count = total_rides_count + 1, total_evaluation_sum = total_evaluation_sum + ? WHERE id = ?",
            )
            .bind(evaluation)
            .bind(&chair_id)
            .execute(&pool)
            .await;
        }
        let _ = user_id;
    });

    let mut b = Buf::new();
    b.obj_open();
    b.key("fare");
    b.i32(fare);
    b.comma();
    b.key("completed_at");
    b.i64(completed_at.timestamp_millis());
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
