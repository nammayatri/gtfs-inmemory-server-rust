use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{error, info};

use crate::tools::error::{AppError, AppResult};

pub const SCHEDULE_SNAPSHOT_TTL: Duration = Duration::from_secs(30);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(5);

#[derive(Debug, sqlx::FromRow)]
pub struct ScheduleTripRow {
    pub route_id: Option<String>,
    pub waybill_id: String,
    pub waybill_no: String,
    pub service_type: String,
    pub vehicle_no: String,
    pub duty_date: Option<String>,
    pub status: Option<String>,
    pub db_start_time: Option<String>,
    pub start_time_epoch: Option<String>,
    pub trip_number: i32,
    pub is_active_trip: Option<bool>,
    pub is_completed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledTrip {
    pub waybill_no: String,
    pub service_type: String,
    pub vehicle_no: String,
    pub duty_date: Option<String>,
    pub status: Option<String>,
    pub db_start_time: Option<String>,
    pub start_time_epoch: Option<String>,
    pub trip_number: Option<i32>,
    pub is_active_trip: Option<bool>,
    pub is_completed: Option<bool>,
}

#[derive(Debug)]
pub struct SnapshotWaybill {
    pub waybill_no: String,
    pub service_type: String,
    pub vehicle_no: String,
    pub duty_date: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug)]
pub struct SnapshotTrip {
    pub waybill: Arc<SnapshotWaybill>,
    pub db_start_time: Option<String>,
    pub start_time_epoch: Option<String>,
    pub trip_number: i32,
    pub is_active_trip: Option<bool>,
    pub is_completed: Option<bool>,
}

impl SnapshotTrip {
    pub fn to_scheduled(&self) -> ScheduledTrip {
        ScheduledTrip {
            waybill_no: self.waybill.waybill_no.clone(),
            service_type: self.waybill.service_type.clone(),
            vehicle_no: self.waybill.vehicle_no.clone(),
            duty_date: self.waybill.duty_date.clone(),
            status: self.waybill.status.clone(),
            db_start_time: self.db_start_time.clone(),
            start_time_epoch: self.start_time_epoch.clone(),
            trip_number: Some(self.trip_number),
            is_active_trip: self.is_active_trip,
            is_completed: self.is_completed,
        }
    }
}

#[derive(Debug, Default)]
pub struct ScheduleSnapshot {
    trips: Vec<SnapshotTrip>,
    by_route: HashMap<String, Vec<usize>>,
    by_waybill: HashMap<String, Vec<usize>>,
}

impl ScheduleSnapshot {
    pub fn new(mut rows: Vec<ScheduleTripRow>) -> Self {
        rows.sort_by(|a, b| {
            a.waybill_no
                .cmp(&b.waybill_no)
                .then_with(|| a.trip_number.cmp(&b.trip_number))
        });

        let mut waybills: HashMap<String, Arc<SnapshotWaybill>> = HashMap::new();
        let mut snapshot = Self {
            trips: Vec::with_capacity(rows.len()),
            ..Self::default()
        };
        for (i, row) in rows.into_iter().enumerate() {
            let waybill = waybills
                .entry(row.waybill_id)
                .or_insert_with(|| {
                    Arc::new(SnapshotWaybill {
                        waybill_no: row.waybill_no,
                        service_type: row.service_type,
                        vehicle_no: row.vehicle_no,
                        duty_date: row.duty_date,
                        status: row.status,
                    })
                })
                .clone();
            if let Some(route_id) = row.route_id {
                snapshot.by_route.entry(route_id).or_default().push(i);
            }
            snapshot
                .by_waybill
                .entry(waybill.waybill_no.clone())
                .or_default()
                .push(i);
            snapshot.trips.push(SnapshotTrip {
                waybill,
                db_start_time: row.db_start_time,
                start_time_epoch: row.start_time_epoch,
                trip_number: row.trip_number,
                is_active_trip: row.is_active_trip,
                is_completed: row.is_completed,
            });
        }
        snapshot
    }

    pub fn len(&self) -> usize {
        self.trips.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trips.is_empty()
    }

    pub fn route_trips<'a>(&'a self, route_id: &str) -> impl Iterator<Item = &'a SnapshotTrip> {
        self.indexed(self.by_route.get(route_id))
    }

    pub fn waybill_trips<'a>(
        &'a self,
        waybill_no: &str,
        trip_number: i32,
    ) -> impl Iterator<Item = &'a SnapshotTrip> {
        self.indexed(self.by_waybill.get(waybill_no))
            .filter(move |t| t.trip_number == trip_number)
    }

    fn indexed<'a>(
        &'a self,
        ids: Option<&'a Vec<usize>>,
    ) -> impl Iterator<Item = &'a SnapshotTrip> {
        ids.into_iter().flatten().map(|&i| &self.trips[i])
    }
}

#[derive(Default)]
struct SlotState {
    snapshot: Option<(Arc<ScheduleSnapshot>, Instant)>,
    failed_at: Option<Instant>,
}

#[derive(Default)]
struct Slot {
    state: Mutex<SlotState>,
    cold_load: tokio::sync::Mutex<()>,
    refreshing: AtomicBool,
}

impl Slot {
    fn current(&self) -> Option<(Arc<ScheduleSnapshot>, Duration)> {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .snapshot
            .as_ref()
            .map(|(snapshot, loaded_at)| (snapshot.clone(), loaded_at.elapsed()))
    }

    fn recently_failed(&self) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state
            .failed_at
            .is_some_and(|at| at.elapsed() < RETRY_AFTER_FAILURE)
    }

    async fn load<F, Fut>(&self, key: &str, load: F) -> AppResult<Arc<ScheduleSnapshot>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = AppResult<Vec<ScheduleTripRow>>>,
    {
        let started = Instant::now();
        let loaded = match tokio::time::timeout(LOAD_TIMEOUT, load()).await {
            Ok(result) => result.map(|rows| Arc::new(ScheduleSnapshot::new(rows))),
            Err(_) => Err(AppError::Internal(format!(
                "schedule snapshot {} load timed out after {:?}",
                key, LOAD_TIMEOUT
            ))),
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match loaded {
            Ok(snapshot) => {
                info!(
                    "schedule snapshot {} refreshed: {} trips in {:?}",
                    key,
                    snapshot.len(),
                    started.elapsed()
                );
                state.snapshot = Some((snapshot.clone(), Instant::now()));
                state.failed_at = None;
                Ok(snapshot)
            }
            Err(e) => {
                state.failed_at = Some(Instant::now());
                Err(e)
            }
        }
    }
}

struct RefreshingGuard(Arc<Slot>);

impl Drop for RefreshingGuard {
    fn drop(&mut self) {
        self.0.refreshing.store(false, Ordering::Release);
    }
}

pub struct ScheduleSnapshotCache {
    ttl: Duration,
    max_staleness: Duration,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

impl ScheduleSnapshotCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            max_staleness: ttl * 2,
            slots: Mutex::new(HashMap::new()),
        }
    }

    pub async fn get<F, Fut>(&self, key: &str, load: F) -> AppResult<Arc<ScheduleSnapshot>>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<Vec<ScheduleTripRow>>> + Send + 'static,
    {
        let slot = self.slot(key);
        if let Some((snapshot, age)) = slot.current() {
            if age < self.ttl {
                return Ok(snapshot);
            }
            if age < self.max_staleness {
                Self::refresh_in_background(slot, key.to_string(), load);
                return Ok(snapshot);
            }
        }

        let _cold_load = slot.cold_load.lock().await;
        if let Some((snapshot, age)) = slot.current() {
            if age < self.max_staleness {
                return Ok(snapshot);
            }
        }
        if slot.recently_failed() {
            return Err(AppError::Internal(format!(
                "schedule snapshot {} is unavailable",
                key
            )));
        }
        slot.load(key, load).await
    }

    fn slot(&self, key: &str) -> Arc<Slot> {
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        slots.entry(key.to_string()).or_default().clone()
    }

    fn refresh_in_background<F, Fut>(slot: Arc<Slot>, key: String, load: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = AppResult<Vec<ScheduleTripRow>>> + Send + 'static,
    {
        if slot.recently_failed() || slot.refreshing.swap(true, Ordering::AcqRel) {
            return;
        }
        let guard = RefreshingGuard(slot);
        tokio::spawn(async move {
            if let Err(e) = guard.0.load(&key, load).await {
                error!(
                    "schedule snapshot {} refresh failed, serving previous one: {}",
                    key, e
                );
            }
        });
    }
}
