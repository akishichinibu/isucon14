use chrono::Utc;
use ulid::Ulid;

use crate::http::HttpResp;
use crate::json::Buf;
use crate::{require_chair, AppState, Error, NOTIFICATION_RETRY_AFTER_MS};

pub async fn chair_post_chairs(state: &AppState, body: &[u8]) -> Result<HttpResp, Error> {
    let req = crate::json::parse_chair_reg(body)?;
    let owner_id = state
        .world
        .read()
        .unwrap()
        .owner_id_by_register(&req.chair_register_token)
        .map(str::to_string)
        .ok_or(Error::Unauthorized("invalid chair_register_token"))?;

    let chair_id = Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let speed: i32 = sqlx::query_scalar("SELECT speed FROM chair_models WHERE name = ?")
        .bind(&req.model)
        .fetch_optional(&state.pool)
        .await?
        .unwrap_or(1);

    sqlx::query(
        "INSERT INTO chairs (id, owner_id, name, model, is_active, access_token) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(&chair_id)
    .bind(&owner_id)
    .bind(&req.name)
    .bind(&req.model)
    .bind(false)
    .bind(&access_token)
    .execute(&state.pool)
    .await?;

    state.world.write().unwrap().upsert_chair(
        crate::state::ChairMem {
            id: chair_id.clone(),
            owner_id: owner_id.clone(),
            name: req.name,
            model: req.model,
            speed: speed.max(1),
            is_active: false,
            created_at: Utc::now(),
            latitude: None,
            longitude: None,
            total_distance: 0,
            total_distance_updated_at: None,
            current_ride: None,
            completed_unsent: std::sync::atomic::AtomicBool::new(false),
            total_rides_count: 0,
            total_evaluation_sum: 0,
            accrued_sales: 0,
        },
        access_token.clone(),
    );

    let mut b = Buf::new();
    b.obj_open();
    b.key("id");
    b.str(&chair_id);
    b.comma();
    b.key("owner_id");
    b.str(&owner_id);
    b.obj_close();
    Ok(HttpResp::json(201, b.into_bytes()).cookie("chair_session", access_token))
}

pub async fn chair_post_activity(
    state: &AppState,
    token: Option<&str>,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let chair_id = require_chair(state, token).await?;
    let is_active = crate::json::parse_named_bool(body, "is_active")?;
    sqlx::query("UPDATE chairs SET is_active = ? WHERE id = ?")
        .bind(is_active)
        .bind(&chair_id)
        .execute(&state.pool)
        .await?;
    state
        .world
        .write()
        .unwrap()
        .set_active(&chair_id, is_active);

    if is_active {
        crate::spawn_matching(state.clone());
    }
    Ok(HttpResp::empty(204))
}

pub async fn chair_post_coordinate(
    state: &AppState,
    token: Option<&str>,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let chair_id = require_chair(state, token).await?;
    let (lat, lng) = crate::json::parse_coordinate(body)?;
    let now = Utc::now();
    let _transition = state
        .world
        .write()
        .unwrap()
        .update_coord(&chair_id, lat, lng, now);

    let mut b = Buf::new();
    b.obj_open();
    b.key("recorded_at");
    b.i64(now.timestamp_millis());
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn chair_get_notification(
    state: &AppState,
    token: Option<&str>,
) -> Result<HttpResp, Error> {
    let chair_id = require_chair(state, token).await?;
    let snapshot = {
        let world = state.world.read().unwrap();
        world
            .pop_chair_event(&chair_id)
            .or_else(|| world.peek_chair_snapshot(&chair_id))
    };
    let mut b = Buf::new();
    b.obj_open();
    b.key("data");
    let Some(ev) = snapshot else {
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
    let user = world.users.get(&ride.user_id);
    let name = user
        .map(|u| format!("{} {}", u.firstname, u.lastname))
        .unwrap_or_default();
    b.obj_open();
    b.key("ride_id");
    b.str(&ride.id);
    b.comma();
    b.key("user");
    b.obj_open();
    b.key("id");
    b.str(&ride.user_id);
    b.comma();
    b.key("name");
    b.str(&name);
    b.obj_close();
    b.comma();
    b.key("pickup_coordinate");
    coord(&mut b, ride.pickup_latitude, ride.pickup_longitude);
    b.comma();
    b.key("destination_coordinate");
    coord(&mut b, ride.destination_latitude, ride.destination_longitude);
    b.comma();
    b.key("status");
    b.str(&ev.status);
    b.obj_close();
    drop(world);
    b.comma();
    b.key("retry_after_ms");
    b.i32(NOTIFICATION_RETRY_AFTER_MS);
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
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

pub async fn chair_post_ride_status(
    state: &AppState,
    token: Option<&str>,
    ride_id: &str,
    body: &[u8],
) -> Result<HttpResp, Error> {
    let chair_id = require_chair(state, token).await?;
    let status = crate::json::parse_named_string(body, "status")?;
    {
        let mut world = state.world.write().unwrap();
        let current = world
            .chairs
            .get(&chair_id)
            .and_then(|c| c.current_ride.clone());
        let Some(current) = current else {
            return Err(Error::BadRequest("not assigned to this ride"));
        };
        if current.ride_id != ride_id {
            return Err(Error::BadRequest("not assigned to this ride"));
        }
        match status.as_str() {
            "ENROUTE" => world.push_status(ride_id, "ENROUTE"),
            "CARRYING" => {
                if current.status != "PICKUP" {
                    return Err(Error::BadRequest("chair has not arrived yet"));
                }
                world.push_status(ride_id, "CARRYING");
            }
            _ => return Err(Error::BadRequest("invalid status")),
        }
    }
    Ok(HttpResp::empty(204))
}
