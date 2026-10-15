use crate::environment::AppConfig;
use crate::models::MinimalEmployee;
use crate::tools::error::{AppError, AppResult};
use async_trait::async_trait;
use sqlx::postgres::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::RwLock;
use tracing::debug;

const EMPLOYEE_MISS_CACHE_SECS: u64 = 300;
const EMPLOYEE_MISS_CACHE_MAX_ENTRIES: usize = 10000;

#[async_trait]
pub trait EmployeeReader: Send + Sync {
    async fn get_employee_by_phone(&self, phone: &str) -> AppResult<Option<MinimalEmployee>>;
}

pub struct MockEmployeeReader;

impl Default for MockEmployeeReader {
    fn default() -> Self {
        Self::new()
    }
}

impl MockEmployeeReader {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl EmployeeReader for MockEmployeeReader {
    async fn get_employee_by_phone(&self, _phone: &str) -> AppResult<Option<MinimalEmployee>> {
        Err(AppError::NotFound(
            "Employee lookup disabled in mock mode".into(),
        ))
    }
}

pub struct DBEmployeeReader {
    pool: PgPool,
    internal_pool: Option<PgPool>,
    cache: Arc<RwLock<HashMap<String, (MinimalEmployee, SystemTime)>>>,
    cache_duration: Duration,
    miss_cache: Arc<RwLock<HashMap<String, SystemTime>>>,
}

impl DBEmployeeReader {
    pub fn new(pool: PgPool, internal_pool: Option<PgPool>, config: &AppConfig) -> Self {
        Self {
            pool,
            internal_pool,
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(config.cache_duration),
            miss_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn is_cached_miss(&self, phone: &str) -> bool {
        let miss_cache = self.miss_cache.read().await;
        miss_cache
            .get(phone)
            .map(|t| {
                t.elapsed().unwrap_or_default() < Duration::from_secs(EMPLOYEE_MISS_CACHE_SECS)
            })
            .unwrap_or(false)
    }

    async fn cache_miss(&self, phone: &str) {
        let mut miss_cache = self.miss_cache.write().await;
        if miss_cache.len() >= EMPLOYEE_MISS_CACHE_MAX_ENTRIES && !miss_cache.contains_key(phone) {
            miss_cache.retain(|_, t| {
                t.elapsed().unwrap_or_default() < Duration::from_secs(EMPLOYEE_MISS_CACHE_SECS)
            });
            if miss_cache.len() >= EMPLOYEE_MISS_CACHE_MAX_ENTRIES {
                miss_cache.clear();
            }
        }
        miss_cache.insert(phone.to_string(), SystemTime::now());
    }

    async fn get_cached_employee(&self, phone: &str) -> Option<MinimalEmployee> {
        let cache = self.cache.read().await;
        if let Some((employee, timestamp)) = cache.get(phone) {
            if timestamp.elapsed().unwrap_or_default() < self.cache_duration {
                debug!("Cache HIT for employee with phone {}", phone);
                return Some(employee.clone());
            }
        }
        debug!("Cache MISS for employee with phone {}", phone);
        None
    }

    async fn cache_employee_data(&self, employee: &MinimalEmployee, phone: &str) {
        let mut cache = self.cache.write().await;
        cache.insert(phone.to_string(), (employee.clone(), SystemTime::now()));
    }
}

#[async_trait]
impl EmployeeReader for DBEmployeeReader {
    async fn get_employee_by_phone(&self, phone: &str) -> AppResult<Option<MinimalEmployee>> {
        if let Some(cached_employee) = self.get_cached_employee(phone).await {
            return Ok(Some(cached_employee));
        }
        if self.is_cached_miss(phone).await {
            debug!("Miss cache HIT for employee with phone {}", phone);
            return Ok(None);
        }

        // Primary lookup in employees table
        let primary_query = r#"
            SELECT
                e.token_no,
                e.first_name,
                e.last_name,
                e.mobile_no,
                ent.entity_remark AS depot_name
            FROM employees e
            LEFT JOIN entities ent ON ent.entity_id = e.entity_id
            WHERE e.mobile_no = $1
              AND e.deleted = false
            LIMIT 1
        "#;

        match sqlx::query_as::<_, MinimalEmployee>(primary_query)
            .bind(phone)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(emp)) => {
                debug!("Employee found in employees table for phone {}", phone);
                self.cache_employee_data(&emp, phone).await;
                return Ok(Some(emp));
            }
            Ok(None) => {}
            Err(e) => return Err(AppError::Internal(format!("DB query error: {}", e))),
        }

        // Fallback lookup in employees_internal table
        let internal_pool = match &self.internal_pool {
            Some(p) => p,
            None => {
                self.cache_miss(phone).await;
                return Ok(None);
            }
        };

        let internal_query = r#"
            SELECT
                e.token_no,
                e.first_name,
                e.last_name,
                e.mobile_no,
                ent.entity_remark AS depot_name
            FROM employees_internal e
            LEFT JOIN entities_internal ent ON ent.entity_id = e.entity_id
            WHERE e.mobile_no = $1
              AND e.deleted = false
            LIMIT 1
        "#;

        match sqlx::query_as::<_, MinimalEmployee>(internal_query)
            .bind(phone)
            .fetch_optional(internal_pool)
            .await
        {
            Ok(Some(emp)) => {
                debug!(
                    "Employee found in employees_internal table for phone {}",
                    phone
                );
                self.cache_employee_data(&emp, phone).await;
                Ok(Some(emp))
            }
            Ok(None) => {
                self.cache_miss(phone).await;
                Ok(None)
            }
            Err(e) => Err(AppError::Internal(format!(
                "DB query error (internal): {}",
                e
            ))),
        }
    }
}
