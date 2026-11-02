use chrono::{DateTime, NaiveDate, Utc};

use crate::http::{query_i64, HttpResp};
use crate::json::Buf;
use crate::{require_owner, AppState, Error};

pub async fn owner_post_owners(state: &AppState, body: &[u8]) -> Result<HttpResp, Error> {
    let name = crate::json::parse_named_string(body, "name")?;
    if name.is_empty() {
        return Err(Error::BadRequest("some of required fields(name) are empty"));
    }
    let owner_id = ulid::Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let chair_register_token = crate::secure_random_str(32);

    sqlx::query(
        "INSERT INTO owners (id, name, access_token, chair_register_token) VALUES (?, ?, ?, ?)",
    )
    .bind(&owner_id)
    .bind(&name)
    .bind(&access_token)
    .bind(&chair_register_token)
    .execute(&state.pool)
    .await?;

    state.world.write().unwrap().remember_owner(
        owner_id.clone(),
        name,
        access_token.clone(),
        chair_register_token.clone(),
    );

    let mut b = Buf::new();
    b.obj_open();
    b.key("id");
    b.str(&owner_id);
    b.comma();
    b.key("chair_register_token");
    b.str(&chair_register_token);
    b.obj_close();
    Ok(HttpResp::json(201, b.into_bytes()).cookie("owner_session", access_token))
}

fn sales_until(until_millis: Option<i64>) -> DateTime<Utc> {
    if let Some(until) = until_millis.and_then(DateTime::from_timestamp_millis) {
        return until + chrono::Duration::microseconds(999);
    }
    NaiveDate::from_ymd_opt(9999, 12, 31)
        .unwrap()
        .and_hms_opt(23, 59, 59)
        .unwrap()
        .and_utc()
}

pub async fn owner_get_sales(
    state: &AppState,
    token: Option<&str>,
    query: &[(String, String)],
) -> Result<HttpResp, Error> {
    let owner_id = require_owner(state, token).await?;
    let since = query_i64(query, "since")
        .and_then(DateTime::from_timestamp_millis)
        .unwrap_or_else(|| DateTime::from_timestamp_millis(0).unwrap());
    let until = sales_until(query_i64(query, "until"));

    let (total, chair_sales, by_model) = state
        .world
        .read()
        .unwrap()
        .owner_sales(&owner_id, since, until);

    let mut b = Buf::new();
    b.obj_open();
    b.key("total_sales");
    b.i32(total);
    b.comma();
    b.key("chairs");
    b.arr_open();
    for (i, (chair, sales)) in chair_sales.iter().enumerate() {
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
        b.key("sales");
        b.i32(*sales);
        b.obj_close();
    }
    b.arr_close();
    b.comma();
    b.key("models");
    b.arr_open();
    for (i, (model, sales)) in by_model.iter().enumerate() {
        if i > 0 {
            b.comma();
        }
        b.obj_open();
        b.key("model");
        b.str(model);
        b.comma();
        b.key("sales");
        b.i32(*sales);
        b.obj_close();
    }
    b.arr_close();
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}

pub async fn owner_get_chairs(state: &AppState, token: Option<&str>) -> Result<HttpResp, Error> {
    let owner_id = require_owner(state, token).await?;
    let chairs = state.world.read().unwrap().owner_chairs(&owner_id);
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
        b.key("active");
        b.bool(chair.is_active);
        b.comma();
        b.key("registered_at");
        b.i64(chair.created_at.timestamp_millis());
        b.comma();
        b.key("total_distance");
        b.i64(chair.total_distance as i64);
        if let Some(t) = chair.total_distance_updated_at {
            b.comma();
            b.key("total_distance_updated_at");
            b.i64(t.timestamp_millis());
        }
        b.obj_close();
    }
    b.arr_close();
    b.obj_close();
    Ok(HttpResp::json(200, b.into_bytes()))
}
