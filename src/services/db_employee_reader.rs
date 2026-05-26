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
}

impl DBEmployeeReader {
    pub fn new(pool: PgPool, internal_pool: Option<PgPool>, config: &AppConfig) -> Self {
        Self {
            pool,
            internal_pool,
            cache: Arc::new(RwLock::new(HashMap::new())),
            cache_duration: Duration::from_secs(config.cache_duration),
        }
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
            None => return Ok(None),
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
            Ok(None) => Ok(None),
            Err(e) => Err(AppError::Internal(format!(
                "DB query error (internal): {}",
                e
            ))),
        }
    }
}
