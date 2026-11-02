use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use chrono::{DateTime, Utc};

use crate::calculate_distance;

#[derive(Debug, Clone)]
pub struct ChairRideHint {
    pub ride_id: String,
    pub pickup_latitude: i32,
    pub pickup_longitude: i32,
    pub destination_latitude: i32,
    pub destination_longitude: i32,
    pub status: String,
}

#[derive(Debug)]
pub struct ChairMem {
    pub id: String,
    pub owner_id: String,
    pub name: String,
    pub model: String,
    pub speed: i32,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub latitude: Option<i32>,
    pub longitude: Option<i32>,
    pub total_distance: i32,
    pub total_distance_updated_at: Option<DateTime<Utc>>,
    pub current_ride: Option<ChairRideHint>,
    pub completed_unsent: AtomicBool,
    pub total_rides_count: i32,
    pub total_evaluation_sum: i32,
    pub accrued_sales: i32,
}

impl Clone for ChairMem {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            owner_id: self.owner_id.clone(),
            name: self.name.clone(),
            model: self.model.clone(),
            speed: self.speed,
            is_active: self.is_active,
            created_at: self.created_at,
            latitude: self.latitude,
            longitude: self.longitude,
            total_distance: self.total_distance,
            total_distance_updated_at: self.total_distance_updated_at,
            current_ride: self.current_ride.clone(),
            completed_unsent: AtomicBool::new(self.completed_unsent.load(Ordering::Relaxed)),
            total_rides_count: self.total_rides_count,
            total_evaluation_sum: self.total_evaluation_sum,
            accrued_sales: self.accrued_sales,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UnmatchedRide {
    pub id: String,
    pub pickup_latitude: i32,
    pub pickup_longitude: i32,
    pub destination_latitude: i32,
    pub destination_longitude: i32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct RideMem {
    pub id: String,
    pub user_id: String,
    pub chair_id: Option<String>,
    pub pickup_latitude: i32,
    pub pickup_longitude: i32,
    pub destination_latitude: i32,
    pub destination_longitude: i32,
    pub status: String,
    pub fare: i32,
    pub evaluation: i32,
    pub discount: i32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct StatusEvent {
    pub ride_id: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct UserMem {
    pub firstname: String,
    pub lastname: String,
}

#[derive(Debug, Clone)]
pub struct CouponMem {
    pub code: String,
    pub discount: i32,
    pub used_by: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct OwnerMem {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Default)]
pub struct World {
    pub paused: bool,
    pub chairs: HashMap<String, ChairMem>,
    pub unmatched: VecDeque<UnmatchedRide>,
    pub rides: HashMap<String, RideMem>,
    pub users: HashMap<String, UserMem>,
    pub owners: HashMap<String, OwnerMem>,
    pub app_notify: Mutex<HashMap<String, VecDeque<StatusEvent>>>,
    pub chair_notify: Mutex<HashMap<String, VecDeque<StatusEvent>>>,
    pub user_by_token: HashMap<String, String>,
    pub chair_by_token: HashMap<String, String>,
    pub owner_by_token: HashMap<String, String>,
    pub owner_by_register: HashMap<String, String>,
    pub coupons: HashMap<String, Vec<CouponMem>>,
    pub payment_tokens: HashMap<String, String>,
    pub payment_gateway_url: String,
    pub model_speed: HashMap<String, i32>,
    pub invitation_by_code: HashMap<String, String>,
}

impl World {
    pub fn remember_user(
        &mut self,
        id: String,
        firstname: String,
        lastname: String,
        token: String,
        invitation_code: String,
    ) {
        if !invitation_code.is_empty() {
            self.invitation_by_code
                .insert(invitation_code, id.clone());
        }
        self.user_by_token.insert(token, id.clone());
        self.users.insert(id, UserMem { firstname, lastname });
    }

    pub fn peek_discount(&self, user_id: &str) -> i32 {
        let Some(cs) = self.coupons.get(user_id) else {
            return 0;
        };
        let ride_count = self
            .rides
            .values()
            .filter(|r| r.user_id == user_id)
            .count();
        if ride_count == 0 {
            if let Some(c) = cs
                .iter()
                .find(|c| c.code == "CP_NEW2024" && c.used_by.is_none())
            {
                return c.discount;
            }
        }
        cs.iter()
            .filter(|c| c.used_by.is_none())
            .min_by_key(|c| c.created_at)
            .map(|c| c.discount)
            .unwrap_or(0)
    }

    pub fn consume_coupon(&mut self, user_id: &str, ride_id: &str) -> i32 {
        let ride_count = self
            .rides
            .values()
            .filter(|r| r.user_id == user_id)
            .count();
        let Some(cs) = self.coupons.get_mut(user_id) else {
            return 0;
        };
        let idx = if ride_count == 0 {
            cs.iter()
                .position(|c| c.code == "CP_NEW2024" && c.used_by.is_none())
                .or_else(|| {
                    cs.iter()
                        .enumerate()
                        .filter(|(_, c)| c.used_by.is_none())
                        .min_by_key(|(_, c)| c.created_at)
                        .map(|(i, _)| i)
                })
        } else {
            cs.iter()
                .enumerate()
                .filter(|(_, c)| c.used_by.is_none())
                .min_by_key(|(_, c)| c.created_at)
                .map(|(i, _)| i)
        };
        if let Some(i) = idx {
            cs[i].used_by = Some(ride_id.to_string());
            cs[i].discount
        } else {
            0
        }
    }

    pub fn add_coupon(&mut self, user_id: String, coupon: CouponMem) {
        self.coupons.entry(user_id).or_default().push(coupon);
    }

    pub fn upsert_chair(&mut self, chair: ChairMem, access_token: String) {
        self.chair_by_token.insert(access_token, chair.id.clone());
        self.chairs.insert(chair.id.clone(), chair);
    }

    pub fn remember_owner(&mut self, id: String, name: String, token: String, register: String) {
        self.owner_by_token.insert(token, id.clone());
        self.owner_by_register.insert(register, id.clone());
        self.owners.insert(id.clone(), OwnerMem { id, name });
    }

    pub fn user_id_by_token(&self, token: &str) -> Option<&str> {
        self.user_by_token.get(token).map(|s| s.as_str())
    }

    pub fn chair_id_by_token(&self, token: &str) -> Option<&str> {
        self.chair_by_token.get(token).map(|s| s.as_str())
    }

    pub fn owner_id_by_token(&self, token: &str) -> Option<&str> {
        self.owner_by_token.get(token).map(|s| s.as_str())
    }

    pub fn owner_id_by_register(&self, token: &str) -> Option<&str> {
        self.owner_by_register.get(token).map(|s| s.as_str())
    }

    pub fn update_coord(
        &mut self,
        chair_id: &str,
        lat: i32,
        lng: i32,
        at: DateTime<Utc>,
    ) -> Option<ChairRideHint> {
        let chair = self.chairs.get_mut(chair_id)?;
        if let (Some(prev_lat), Some(prev_lng)) = (chair.latitude, chair.longitude) {
            chair.total_distance += calculate_distance(prev_lat, prev_lng, lat, lng);
        }
        chair.latitude = Some(lat);
        chair.longitude = Some(lng);
        chair.total_distance_updated_at = Some(at);

        let hint = chair.current_ride.clone()?;
        if hint.status == "ENROUTE" && lat == hint.pickup_latitude && lng == hint.pickup_longitude {
            self.push_status(&hint.ride_id, "PICKUP");
            return self
                .chairs
                .get(chair_id)
                .and_then(|c| c.current_ride.clone());
        }
        if hint.status == "CARRYING"
            && lat == hint.destination_latitude
            && lng == hint.destination_longitude
        {
            self.push_status(&hint.ride_id, "ARRIVED");
            return self
                .chairs
                .get(chair_id)
                .and_then(|c| c.current_ride.clone());
        }
        None
    }

    pub fn set_active(&mut self, chair_id: &str, is_active: bool) {
        if let Some(c) = self.chairs.get_mut(chair_id) {
            c.is_active = is_active;
        }
    }

    pub fn push_unmatched(&mut self, ride: UnmatchedRide) {
        self.unmatched.push_back(ride);
    }

    pub fn add_ride(&mut self, ride: RideMem) {
        self.app_notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(ride.user_id.clone())
            .or_default()
            .push_back(StatusEvent {
                ride_id: ride.id.clone(),
                status: ride.status.clone(),
            });
        self.rides.insert(ride.id.clone(), ride);
    }

    pub fn push_status(&mut self, ride_id: &str, status: &str) {
        let now = Utc::now();
        let Some(ride) = self.rides.get_mut(ride_id) else {
            return;
        };
        ride.status = status.to_string();
        ride.updated_at = now;
        let user_id = ride.user_id.clone();
        let chair_id = ride.chair_id.clone();
        if let Some(cid) = &chair_id {
            if let Some(c) = self.chairs.get_mut(cid) {
                if let Some(cur) = c.current_ride.as_mut() {
                    if cur.ride_id == ride_id {
                        cur.status = status.to_string();
                    }
                }
            }
        }
        self.app_notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(user_id)
            .or_default()
            .push_back(StatusEvent {
                ride_id: ride_id.to_string(),
                status: status.to_string(),
            });
        if let Some(cid) = chair_id {
            self.chair_notify
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(cid)
                .or_default()
                .push_back(StatusEvent {
                    ride_id: ride_id.to_string(),
                    status: status.to_string(),
                });
        }
    }

    pub fn assign(&mut self, ride_id: &str, chair_id: &str, hint: ChairRideHint) {
        self.unmatched.retain(|r| r.id != ride_id);
        if let Some(ride) = self.rides.get(ride_id) {
            if ride.status == "COMPLETED" || ride.chair_id.is_some() {
                return;
            }
        }
        if let Some(ride) = self.rides.get_mut(ride_id) {
            ride.chair_id = Some(chair_id.to_string());
            ride.updated_at = Utc::now();
        }
        if let Some(c) = self.chairs.get_mut(chair_id) {
            c.current_ride = Some(hint.clone());
            c.completed_unsent.store(false, Ordering::Relaxed);
        }
        // User already has MATCHING; push again so the payload includes the chair.
        self.push_status(ride_id, "MATCHING");
        // push_status also enqueues chair MATCHING.
    }

    pub fn complete_ride(
        &mut self,
        ride_id: &str,
        chair_id: Option<&str>,
        evaluation: i32,
        completed_at: DateTime<Utc>,
    ) {
        let chair_id = chair_id
            .map(str::to_string)
            .or_else(|| self.rides.get(ride_id).and_then(|r| r.chair_id.clone()));
        let fare = self
            .rides
            .get(ride_id)
            .map(|r| {
                crate::calculate_fare(
                    r.pickup_latitude,
                    r.pickup_longitude,
                    r.destination_latitude,
                    r.destination_longitude,
                )
            })
            .unwrap_or(0);
        if let Some(cid) = chair_id.as_ref() {
            if let Some(ride) = self.rides.get_mut(ride_id) {
                if ride.chair_id.is_none() {
                    ride.chair_id = Some(cid.clone());
                }
            }
        }
        self.push_status(ride_id, "COMPLETED");
        // Match evaluate's completed_at (rides.updated_at). Sales until-filter
        // uses this timestamp; Utc::now() here is later and drops the last ride.
        if let Some(ride) = self.rides.get_mut(ride_id) {
            ride.updated_at = completed_at;
            ride.status = "COMPLETED".to_string();
            ride.evaluation = evaluation;
        }
        if let Some(cid) = chair_id {
            if let Some(c) = self.chairs.get_mut(&cid) {
                if c.current_ride
                    .as_ref()
                    .map(|h| h.ride_id == ride_id)
                    .unwrap_or(true)
                {
                    c.current_ride = None;
                    c.completed_unsent.store(true, Ordering::Relaxed);
                }
                c.total_rides_count += 1;
                c.total_evaluation_sum += evaluation;
                c.accrued_sales += fare;
            }
        }
    }

    pub fn mark_completed_sent(&self, chair_id: &str) {
        if let Some(c) = self.chairs.get(chair_id) {
            c.completed_unsent.store(false, Ordering::Relaxed);
        }
    }

    pub fn pop_chair_event(&self, chair_id: &str) -> Option<StatusEvent> {
        let ev = {
            let mut map = self.chair_notify.lock().unwrap_or_else(|e| e.into_inner());
            map.get_mut(chair_id)?.pop_front()
        }?;
        if ev.status == "COMPLETED" {
            self.mark_completed_sent(chair_id);
        }
        Some(ev)
    }

    pub fn peek_chair_snapshot(&self, chair_id: &str) -> Option<StatusEvent> {
        {
            let map = self.chair_notify.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(q) = map.get(chair_id) {
                if let Some(front) = q.front() {
                    return Some(front.clone());
                }
            }
        }
        self.chairs
            .get(chair_id)
            .and_then(|c| c.current_ride.as_ref())
            .map(|h| StatusEvent {
                ride_id: h.ride_id.clone(),
                status: h.status.clone(),
            })
    }

    pub fn pop_app_event(&self, user_id: &str) -> Option<StatusEvent> {
        self.app_notify
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(user_id)?
            .pop_front()
    }

    pub fn peek_app_snapshot(&self, user_id: &str) -> Option<StatusEvent> {
        {
            let map = self.app_notify.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(q) = map.get(user_id) {
                if let Some(front) = q.front() {
                    return Some(front.clone());
                }
            }
        }
        self.rides
            .values()
            .filter(|r| r.user_id == user_id)
            .max_by_key(|r| r.created_at)
            .map(|r| StatusEvent {
                ride_id: r.id.clone(),
                status: r.status.clone(),
            })
    }

    pub fn nearby(&self, lat: i32, lng: i32, distance: i32) -> Vec<ChairMem> {
        self.chairs
            .values()
            .filter(|c| {
                if !c.is_active || c.latitude.is_none() || c.longitude.is_none() {
                    return false;
                }
                c.current_ride.is_none()
                    && !c.completed_unsent.load(Ordering::Relaxed)
                    && calculate_distance(lat, lng, c.latitude.unwrap(), c.longitude.unwrap())
                        <= distance
            })
            .cloned()
            .collect()
    }

    pub fn owner_chairs(&self, owner_id: &str) -> Vec<ChairMem> {
        self.chairs
            .values()
            .filter(|c| c.owner_id == owner_id)
            .cloned()
            .collect()
    }

    pub fn owner_sales(
        &self,
        owner_id: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> (
        i32,
        Vec<(ChairMem, i32)>,
        std::collections::HashMap<String, i32>,
    ) {
        let chairs: Vec<ChairMem> = self
            .chairs
            .values()
            .filter(|c| c.owner_id == owner_id)
            .cloned()
            .collect();
        let chair_ids: std::collections::HashSet<String> =
            chairs.iter().map(|c| c.id.clone()).collect();
        let mut by_chair: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
        for ride in self.rides.values() {
            if ride.status != "COMPLETED" {
                continue;
            }
            if ride.updated_at < since || ride.updated_at > until {
                continue;
            }
            let Some(cid) = ride.chair_id.as_ref() else {
                continue;
            };
            if !chair_ids.contains(cid) {
                continue;
            }
            // Owner sales are undiscounted (initial + distance), not the user fare.
            *by_chair.entry(cid.clone()).or_insert(0) += crate::calculate_fare(
                ride.pickup_latitude,
                ride.pickup_longitude,
                ride.destination_latitude,
                ride.destination_longitude,
            );
        }
        let mut total = 0;
        let mut chair_sales = Vec::with_capacity(chairs.len());
        let mut by_model: std::collections::HashMap<String, i32> = std::collections::HashMap::new();
        for chair in chairs {
            let s = *by_chair.get(&chair.id).unwrap_or(&0);
            total += s;
            *by_model.entry(chair.model.clone()).or_insert(0) += s;
            chair_sales.push((chair, s));
        }
        (total, chair_sales, by_model)
    }

    pub fn match_once(&mut self) -> Vec<(String, String, ChairRideHint)> {
        if self.paused {
            return Vec::new();
        }
        let mut assignments = Vec::new();
        let mut used_chairs = std::collections::HashSet::new();
        self.unmatched.retain(|u| {
            self.rides
                .get(&u.id)
                .map(|r| r.status == "MATCHING" && r.chair_id.is_none())
                .unwrap_or(false)
        });
        let pending: Vec<UnmatchedRide> = self.unmatched.iter().cloned().collect();

        const NEAR: i32 = 400;
        for ride in pending {
            let mut best_near: Option<(String, i32, i32)> = None;
            let mut best_far: Option<(String, i32, i32)> = None;
            for chair in self.chairs.values() {
                if used_chairs.contains(&chair.id)
                    || !chair.is_active
                    || chair.current_ride.is_some()
                    || chair.completed_unsent.load(Ordering::Relaxed)
                    || chair.speed <= 0
                {
                    continue;
                }
                let dist = match (chair.latitude, chair.longitude) {
                    (Some(lat), Some(lng)) => {
                        calculate_distance(lat, lng, ride.pickup_latitude, ride.pickup_longitude)
                    }
                    _ => 50_000,
                };
                let better = |best: &Option<(String, i32, i32)>| match best {
                    None => true,
                    Some((_, bd, bs)) => {
                        let eta = dist as f64 / chair.speed as f64;
                        let best_eta = *bd as f64 / *bs as f64;
                        eta < best_eta || (eta == best_eta && dist < *bd)
                    }
                };
                if dist <= NEAR && better(&best_near) {
                    best_near = Some((chair.id.clone(), dist, chair.speed));
                }
                if better(&best_far) {
                    best_far = Some((chair.id.clone(), dist, chair.speed));
                }
            }
            if let Some((chair_id, _, _)) = best_near.or(best_far) {
                used_chairs.insert(chair_id.clone());
                let hint = ChairRideHint {
                    ride_id: ride.id.clone(),
                    pickup_latitude: ride.pickup_latitude,
                    pickup_longitude: ride.pickup_longitude,
                    destination_latitude: ride.destination_latitude,
                    destination_longitude: ride.destination_longitude,
                    status: "MATCHING".to_string(),
                };
                assignments.push((ride.id, chair_id, hint));
            }
        }

        for (ride_id, chair_id, hint) in &assignments {
            self.assign(ride_id, chair_id, hint.clone());
        }
        assignments
    }
}

pub type SharedWorld = std::sync::Arc<std::sync::RwLock<World>>;

#[derive(Debug, sqlx::FromRow)]
struct ChairRow {
    id: String,
    owner_id: String,
    name: String,
    model: String,
    is_active: bool,
    created_at: DateTime<Utc>,
    latitude: Option<i32>,
    longitude: Option<i32>,
    total_distance: i32,
    total_distance_updated_at: Option<DateTime<Utc>>,
    speed: Option<i32>,
    total_rides_count: Option<i32>,
    total_evaluation_sum: Option<i32>,
    access_token: String,
}

#[derive(Debug, sqlx::FromRow)]
struct RideRow {
    id: String,
    user_id: String,
    chair_id: Option<String>,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
    latest_status: String,
    evaluation: Option<i32>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    id: String,
    firstname: String,
    lastname: String,
    access_token: String,
    invitation_code: String,
}

#[derive(Debug, sqlx::FromRow)]
struct OwnerRow {
    id: String,
    name: String,
    access_token: String,
    chair_register_token: String,
}

pub async fn load_world(pool: &sqlx::MySqlPool) -> sqlx::Result<World> {
    let rows: Vec<ChairRow> = sqlx::query_as(
        r#"SELECT c.id, c.owner_id, c.name, c.model, c.is_active, c.created_at,
                  c.latitude, c.longitude, c.total_distance, c.total_distance_updated_at,
                  cm.speed AS speed,
                  c.total_rides_count AS total_rides_count,
                  c.total_evaluation_sum AS total_evaluation_sum,
                  c.access_token AS access_token
           FROM chairs c
           LEFT JOIN chair_models cm ON cm.name = c.model"#,
    )
    .fetch_all(pool)
    .await?;

    let rides: Vec<RideRow> = sqlx::query_as(
        r#"SELECT id, user_id, chair_id, pickup_latitude, pickup_longitude,
                  destination_latitude, destination_longitude, latest_status, evaluation, created_at, updated_at
           FROM rides"#,
    )
    .fetch_all(pool)
    .await?;

    let users: Vec<UserRow> =
        sqlx::query_as("SELECT id, firstname, lastname, access_token, invitation_code FROM users")
            .fetch_all(pool)
            .await?;
    let owners: Vec<OwnerRow> = sqlx::query_as(
        "SELECT id, name, access_token, chair_register_token FROM owners",
    )
    .fetch_all(pool)
    .await?;

    let mut world = World::default();
    let models: Vec<(String, i32)> = sqlx::query_as("SELECT name, speed FROM chair_models")
        .fetch_all(pool)
        .await?;
    for (name, speed) in models {
        world.model_speed.insert(name, speed.max(1));
    }
    if let Ok(url) = sqlx::query_scalar::<_, String>(
        "SELECT value FROM settings WHERE name = 'payment_gateway_url'",
    )
    .fetch_one(pool)
    .await
    {
        world.payment_gateway_url = url;
    }
    let tokens: Vec<(String, String)> =
        sqlx::query_as("SELECT user_id, token FROM payment_tokens")
            .fetch_all(pool)
            .await?;
    for (uid, tok) in tokens {
        world.payment_tokens.insert(uid, tok);
    }
    let coupon_rows: Vec<(String, String, i32, Option<String>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT user_id, code, discount, used_by, created_at FROM coupons",
    )
    .fetch_all(pool)
    .await?;
    for (uid, code, discount, used_by, created_at) in coupon_rows {
        world.add_coupon(
            uid,
            CouponMem {
                code,
                discount,
                used_by,
                created_at,
            },
        );
    }
    for u in users {
        world.remember_user(
            u.id,
            u.firstname,
            u.lastname,
            u.access_token,
            u.invitation_code,
        );
    }
    for o in owners {
        world.remember_owner(o.id, o.name, o.access_token, o.chair_register_token);
    }
    for row in rows {
        world.upsert_chair(
            ChairMem {
                id: row.id,
                owner_id: row.owner_id,
                name: row.name,
                model: row.model,
                speed: row.speed.unwrap_or(1).max(1),
                is_active: row.is_active,
                created_at: row.created_at,
                latitude: row.latitude,
                longitude: row.longitude,
                total_distance: row.total_distance,
                total_distance_updated_at: row.total_distance_updated_at,
                current_ride: None,
                completed_unsent: AtomicBool::new(false),
                total_rides_count: row.total_rides_count.unwrap_or(0),
                total_evaluation_sum: row.total_evaluation_sum.unwrap_or(0),
                accrued_sales: 0,
            },
            row.access_token,
        );
    }

    for ride in rides {
        let mem = RideMem {
            id: ride.id.clone(),
            user_id: ride.user_id.clone(),
            chair_id: ride.chair_id.clone(),
            pickup_latitude: ride.pickup_latitude,
            pickup_longitude: ride.pickup_longitude,
            destination_latitude: ride.destination_latitude,
            destination_longitude: ride.destination_longitude,
            status: ride.latest_status.clone(),
            fare: {
                let discount = world
                    .coupons
                    .values()
                    .flatten()
                    .find(|c| c.used_by.as_deref() == Some(ride.id.as_str()))
                    .map(|c| c.discount)
                    .unwrap_or(0);
                crate::discounted_fare(
                    ride.pickup_latitude,
                    ride.pickup_longitude,
                    ride.destination_latitude,
                    ride.destination_longitude,
                    discount,
                )
            },
            evaluation: ride.evaluation.unwrap_or(0),
            discount: 0,
            created_at: ride.created_at,
            updated_at: ride.updated_at,
        };
        if ride.latest_status == "COMPLETED" {
            world.rides.insert(ride.id, mem);
            continue;
        }
        if let Some(chair_id) = &ride.chair_id {
            if ride.latest_status != "COMPLETED" {
                if let Some(c) = world.chairs.get_mut(chair_id) {
                    c.current_ride = Some(ChairRideHint {
                        ride_id: ride.id.clone(),
                        pickup_latitude: ride.pickup_latitude,
                        pickup_longitude: ride.pickup_longitude,
                        destination_latitude: ride.destination_latitude,
                        destination_longitude: ride.destination_longitude,
                        status: ride.latest_status.clone(),
                    });
                }
            }
        } else {
            world.unmatched.push_back(UnmatchedRide {
                id: ride.id.clone(),
                pickup_latitude: ride.pickup_latitude,
                pickup_longitude: ride.pickup_longitude,
                destination_latitude: ride.destination_latitude,
                destination_longitude: ride.destination_longitude,
                created_at: ride.created_at,
            });
        }
        world.rides.insert(ride.id, mem);
    }

    let accrued: Vec<(String, i32)> = world
        .rides
        .values()
        .filter(|r| r.status == "COMPLETED")
        .filter_map(|r| {
            r.chair_id.clone().map(|cid| {
                (
                    cid,
                    crate::calculate_fare(
                        r.pickup_latitude,
                        r.pickup_longitude,
                        r.destination_latitude,
                        r.destination_longitude,
                    ),
                )
            })
        })
        .collect();
    for (cid, fare) in accrued {
        if let Some(c) = world.chairs.get_mut(&cid) {
            c.accrued_sales += fare;
        }
    }

    Ok(world)
}

pub fn persist_status(pool: sqlx::MySqlPool, ride_id: String, status: String) {
    tokio::spawn(async move {
        let id = ulid::Ulid::new().to_string();
        let _ = sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(&ride_id)
            .bind(&status)
            .execute(&pool)
            .await;
        // Never walk latest_status backwards (async ARRIVED must not clobber COMPLETED).
        let _ = sqlx::query(
            r#"UPDATE rides
               SET latest_status = ?, updated_at = updated_at
               WHERE id = ?
                 AND FIELD(latest_status, 'MATCHING','ENROUTE','PICKUP','CARRYING','ARRIVED','COMPLETED')
                   < FIELD(?, 'MATCHING','ENROUTE','PICKUP','CARRYING','ARRIVED','COMPLETED')"#,
        )
        .bind(&status)
        .bind(&ride_id)
        .bind(&status)
        .execute(&pool)
        .await;
    });
}
