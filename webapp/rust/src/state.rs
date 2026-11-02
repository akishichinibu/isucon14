use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

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

#[derive(Debug, Clone)]
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
    pub completed_unsent: bool,
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

#[derive(Debug, Default)]
pub struct World {
    pub paused: bool,
    pub chairs: HashMap<String, ChairMem>,
    pub unmatched: VecDeque<UnmatchedRide>,
}

impl World {
    pub fn upsert_chair(&mut self, chair: ChairMem) {
        self.chairs.insert(chair.id.clone(), chair);
    }

    pub fn update_coord(&mut self, chair_id: &str, lat: i32, lng: i32, at: DateTime<Utc>) -> Option<ChairRideHint> {
        let chair = self.chairs.get_mut(chair_id)?;
        if let (Some(prev_lat), Some(prev_lng)) = (chair.latitude, chair.longitude) {
            chair.total_distance += calculate_distance(prev_lat, prev_lng, lat, lng);
        }
        chair.latitude = Some(lat);
        chair.longitude = Some(lng);
        chair.total_distance_updated_at = Some(at);

        let hint = chair.current_ride.clone()?;
        if hint.status == "ENROUTE" && lat == hint.pickup_latitude && lng == hint.pickup_longitude {
            if let Some(ride) = chair.current_ride.as_mut() {
                ride.status = "PICKUP".to_string();
            }
            return chair.current_ride.clone();
        }
        if hint.status == "CARRYING"
            && lat == hint.destination_latitude
            && lng == hint.destination_longitude
        {
            if let Some(ride) = chair.current_ride.as_mut() {
                ride.status = "ARRIVED".to_string();
            }
            return chair.current_ride.clone();
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

    pub fn assign(&mut self, ride_id: &str, chair_id: &str, hint: ChairRideHint) {
        self.unmatched.retain(|r| r.id != ride_id);
        if let Some(c) = self.chairs.get_mut(chair_id) {
            c.current_ride = Some(hint);
            c.completed_unsent = false;
        }
    }

    pub fn complete_ride(&mut self, chair_id: &str) {
        if let Some(c) = self.chairs.get_mut(chair_id) {
            c.current_ride = None;
            c.completed_unsent = true;
        }
    }

    pub fn mark_completed_sent(&mut self, chair_id: &str) {
        if let Some(c) = self.chairs.get_mut(chair_id) {
            c.completed_unsent = false;
        }
    }

    pub fn nearby(&self, lat: i32, lng: i32, distance: i32) -> Vec<ChairMem> {
        self.chairs
            .values()
            .filter(|c| {
                // Same rule as the reference SQL: active, has a position, not on an open ride.
                c.is_active
                    && c.current_ride.is_none()
                    && c.latitude.is_some()
                    && c.longitude.is_some()
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

    /// Greedy ETA matching. Returns (ride_id, chair_id, hint).
    pub fn match_once(&mut self) -> Vec<(String, String, ChairRideHint)> {
        if self.paused {
            return Vec::new();
        }
        let mut assignments = Vec::new();
        let mut used_chairs = std::collections::HashSet::new();
        let pending: Vec<UnmatchedRide> = self.unmatched.iter().cloned().collect();

        for ride in pending {
            let mut best: Option<(String, i32, i32)> = None; // chair_id, dist, speed
            for chair in self.chairs.values() {
                if used_chairs.contains(&chair.id)
                    || !chair.is_active
                    || chair.current_ride.is_some()
                    || chair.completed_unsent
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
                let better = match &best {
                    None => true,
                    Some((_, bd, bs)) => {
                        let eta = dist as f64 / chair.speed as f64;
                        let best_eta = *bd as f64 / *bs as f64;
                        eta < best_eta || (eta == best_eta && dist < *bd)
                    }
                };
                if better {
                    best = Some((chair.id.clone(), dist, chair.speed));
                }
            }
            if let Some((chair_id, _, _)) = best {
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

pub type SharedWorld = std::sync::Arc<RwLock<World>>;

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
}

#[derive(Debug, sqlx::FromRow)]
struct RideRow {
    id: String,
    chair_id: Option<String>,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
    latest_status: String,
    created_at: DateTime<Utc>,
}

pub async fn load_world(pool: &sqlx::MySqlPool) -> sqlx::Result<World> {
    let rows: Vec<ChairRow> = sqlx::query_as(
        r#"SELECT c.id, c.owner_id, c.name, c.model, c.is_active, c.created_at,
                  c.latitude, c.longitude, c.total_distance, c.total_distance_updated_at,
                  cm.speed AS speed
           FROM chairs c
           LEFT JOIN chair_models cm ON cm.name = c.model"#,
    )
    .fetch_all(pool)
    .await?;

    let rides: Vec<RideRow> = sqlx::query_as(
        r#"SELECT id, chair_id, pickup_latitude, pickup_longitude,
                  destination_latitude, destination_longitude, latest_status, created_at
           FROM rides
           WHERE chair_id IS NULL OR latest_status <> 'COMPLETED'"#,
    )
    .fetch_all(pool)
    .await?;

    let mut world = World::default();
    for row in rows {
        world.chairs.insert(
            row.id.clone(),
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
                completed_unsent: false,
            },
        );
    }

    for ride in rides {
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
                id: ride.id,
                pickup_latitude: ride.pickup_latitude,
                pickup_longitude: ride.pickup_longitude,
                destination_latitude: ride.destination_latitude,
                destination_longitude: ride.destination_longitude,
                created_at: ride.created_at,
            });
        }
    }

    Ok(world)
}
