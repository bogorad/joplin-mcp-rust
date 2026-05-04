use crate::observability::metrics;
use sqlx::{PgPool, Postgres, pool::PoolConnection};
use std::{future::Future, time::Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostgresPoolRole {
    Runtime,
    Indexer,
}

impl PostgresPoolRole {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Indexer => "indexer",
        }
    }
}

pub async fn acquire_runtime(pool: &PgPool) -> Result<PoolConnection<Postgres>, sqlx::Error> {
    acquire_measured(pool, PostgresPoolRole::Runtime).await
}

pub async fn acquire_indexer(pool: &PgPool) -> Result<PoolConnection<Postgres>, sqlx::Error> {
    acquire_measured(pool, PostgresPoolRole::Indexer).await
}

async fn acquire_measured(
    pool: &PgPool,
    role: PostgresPoolRole,
) -> Result<PoolConnection<Postgres>, sqlx::Error> {
    measure_acquire(pool.acquire(), role, metrics::record_postgres_pool_wait).await
}

async fn measure_acquire<T, E>(
    acquire: impl Future<Output = Result<T, E>>,
    role: PostgresPoolRole,
    record: impl FnOnce(&str, std::time::Duration),
) -> Result<T, E> {
    let started = Instant::now();
    let result = acquire.await;
    record(role.label(), started.elapsed());
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[test]
    fn pool_role_labels_are_bounded_to_metric_contract() {
        assert_eq!(PostgresPoolRole::Runtime.label(), "runtime");
        assert_eq!(PostgresPoolRole::Indexer.label(), "indexer");
        assert_eq!(metrics::POSTGRES_POOLS, ["runtime", "indexer"]);
    }

    #[tokio::test]
    async fn measure_acquire_records_elapsed_runtime_wait() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let records_for_callback = records.clone();

        let result: Result<&str, ()> = measure_acquire(
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Ok("connection")
            },
            PostgresPoolRole::Runtime,
            move |pool, duration| {
                records_for_callback
                    .lock()
                    .expect("records lock")
                    .push((pool.to_string(), duration));
            },
        )
        .await;

        assert_eq!(result.expect("acquire result"), "connection");
        let records = records.lock().expect("records lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, "runtime");
        assert!(records[0].1 >= Duration::from_millis(1));
    }

    #[tokio::test]
    async fn measure_acquire_records_failed_indexer_waits() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let records_for_callback = records.clone();

        let result: Result<(), &str> = measure_acquire(
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                Err("timeout")
            },
            PostgresPoolRole::Indexer,
            move |pool, duration| {
                records_for_callback
                    .lock()
                    .expect("records lock")
                    .push((pool.to_string(), duration));
            },
        )
        .await;

        assert_eq!(result.expect_err("acquire error"), "timeout");
        let records = records.lock().expect("records lock");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].0, "indexer");
        assert!(records[0].1 >= Duration::from_millis(1));
    }
}
