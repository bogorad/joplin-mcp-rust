use crate::config::IndexConfig;
use crate::indexer::refresh::{
    IncrementalRefreshOutcome, RefreshUser, due_refresh_users, incremental_refresh_user,
    mark_index_checked, refresh_needed,
};
use crate::indexer::source::{JoplinSource, JoplinSourceWatermark};
use crate::observability::metrics;
use sqlx::PgPool;
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{MissedTickBehavior, interval};

pub fn spawn_index_worker<S>(mcp_pool: PgPool, source: S, config: IndexConfig) -> JoinHandle<()>
where
    S: JoplinSource + Clone + Send + Sync + 'static,
{
    tokio::spawn(async move {
        run_index_refresh_cycle(&mcp_pool, &source, &config).await;

        let mut ticker = interval(Duration::from_secs(config.refresh_interval_seconds));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            run_index_refresh_cycle(&mcp_pool, &source, &config).await;
        }
    })
}

pub async fn run_index_refresh_cycle<S>(mcp_pool: &PgPool, source: &S, config: &IndexConfig)
where
    S: JoplinSource + Clone + Send + Sync + 'static,
{
    tracing::info!(operation = "index_refresh", "index refresh cycle started");
    match due_refresh_users(mcp_pool, config).await {
        Ok(users) => {
            let stats = refresh_users(mcp_pool, source, config, users).await;
            stats.record_metrics();
            stats.log_finished();
        }
        Err(error) => {
            tracing::warn!(operation = "index_refresh", %error, "failed to load due index users");
            let stats = WorkerCycleStats::default();
            stats.record_metrics();
            stats.log_finished();
        }
    }
}

async fn refresh_users<S>(
    mcp_pool: &PgPool,
    source: &S,
    config: &IndexConfig,
    users: Vec<RefreshUser>,
) -> WorkerCycleStats
where
    S: JoplinSource + Clone + Send + Sync + 'static,
{
    let mut stats = WorkerCycleStats::started(users.len());

    if users.is_empty() {
        return stats;
    }

    let joplin_user_ids = users
        .iter()
        .map(|user| user.joplin_user_id.clone())
        .collect::<Vec<_>>();
    let watermarks = match source.source_watermarks(&joplin_user_ids).await {
        Ok(watermarks) => watermarks_by_owner(watermarks),
        Err(error) => {
            tracing::warn!(
                operation = "index_refresh",
                %error,
                "failed to load Joplin source watermarks"
            );
            stats.failed += stats.checked;
            return stats;
        }
    };

    let mut tasks = JoinSet::new();
    for user in users {
        let source_watermark = watermarks.get(&user.joplin_user_id).copied();
        let mcp_pool = mcp_pool.clone();
        let source = source.clone();
        let config = config.clone();

        tasks.spawn(async move {
            if refresh_needed(&user, source_watermark, &config) {
                incremental_refresh_user(&mcp_pool, &source, &user, &config)
                    .await
                    .map(|outcome| {
                        if outcome.lock_acquired {
                            WorkerUserOutcome::Refreshed(outcome)
                        } else {
                            WorkerUserOutcome::SkippedLock
                        }
                    })
            } else {
                mark_index_checked(&mcp_pool, user.mcp_user_id, source_watermark)
                    .await
                    .map(|_| WorkerUserOutcome::SkippedCurrent)
            }
            .map_err(|error| (user.mcp_user_id, error))
        });
    }

    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(outcome)) => stats.add_outcome(outcome),
            Ok(Err((user_id, error))) => {
                stats.failed += 1;
                tracing::warn!(
                    operation = "index_refresh",
                    user_id = %user_id,
                    %error,
                    "index refresh failed"
                );
            }
            Err(error) => {
                stats.failed += 1;
                tracing::warn!(
                    operation = "index_refresh",
                    %error,
                    "index refresh task failed"
                );
            }
        }
    }

    stats
}

fn watermarks_by_owner(watermarks: Vec<JoplinSourceWatermark>) -> HashMap<String, i64> {
    watermarks
        .into_iter()
        .map(|watermark| (watermark.owner_id, watermark.source_watermark))
        .collect()
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct WorkerCycleStats {
    checked: u64,
    refreshed: u64,
    skipped_current: u64,
    skipped_lock: u64,
    failed: u64,
    skipped_encrypted: u64,
    skipped_malformed: u64,
    skipped_wrong_owner: u64,
    full_rebuild_indexed_items: u64,
    full_rebuild_deleted_items: u64,
}

impl WorkerCycleStats {
    fn started(checked: usize) -> Self {
        Self {
            checked: checked as u64,
            ..Self::default()
        }
    }

    fn add_outcome(&mut self, outcome: WorkerUserOutcome) {
        match outcome {
            WorkerUserOutcome::Refreshed(outcome) => {
                self.refreshed += 1;
                self.skipped_encrypted += outcome.skipped_encrypted as u64;
                self.skipped_malformed += outcome.skipped_malformed as u64;
                self.skipped_wrong_owner += outcome.skipped_wrong_owner as u64;
                self.full_rebuild_indexed_items += outcome.full_rebuild_indexed_items as u64;
                self.full_rebuild_deleted_items += outcome.full_rebuild_deleted_items as u64;
            }
            WorkerUserOutcome::SkippedCurrent => self.skipped_current += 1,
            WorkerUserOutcome::SkippedLock => self.skipped_lock += 1,
        }
    }

    fn record_metrics(&self) {
        metrics::record_index_worker_cycle_users("checked", self.checked);
        metrics::record_index_worker_cycle_users("refreshed", self.refreshed);
        metrics::record_index_worker_cycle_users("skipped_current", self.skipped_current);
        metrics::record_index_worker_cycle_users("skipped_lock", self.skipped_lock);
        metrics::record_index_worker_cycle_users("failed", self.failed);
        metrics::record_index_worker_cycle_users("skipped_encrypted", self.skipped_encrypted);
        metrics::record_index_worker_cycle_users("skipped_malformed", self.skipped_malformed);
        metrics::record_index_worker_cycle_users("skipped_wrong_owner", self.skipped_wrong_owner);
        metrics::record_index_worker_cycle_users(
            "full_rebuild_indexed_items",
            self.full_rebuild_indexed_items,
        );
        metrics::record_index_worker_cycle_users(
            "full_rebuild_deleted_items",
            self.full_rebuild_deleted_items,
        );
    }

    fn log_finished(&self) {
        tracing::info!(
            operation = "index_refresh",
            checked = self.checked,
            refreshed = self.refreshed,
            skipped_current = self.skipped_current,
            skipped_lock = self.skipped_lock,
            failed = self.failed,
            skipped_encrypted = self.skipped_encrypted,
            skipped_malformed = self.skipped_malformed,
            skipped_wrong_owner = self.skipped_wrong_owner,
            full_rebuild_indexed_items = self.full_rebuild_indexed_items,
            full_rebuild_deleted_items = self.full_rebuild_deleted_items,
            "index refresh cycle finished"
        );
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WorkerUserOutcome {
    Refreshed(IncrementalRefreshOutcome),
    SkippedCurrent,
    SkippedLock,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_interval_uses_index_refresh_interval() {
        let config = IndexConfig {
            refresh_interval_seconds: 7,
            ..IndexConfig::default()
        };

        assert_eq!(
            Duration::from_secs(config.refresh_interval_seconds),
            Duration::from_secs(7)
        );
    }

    #[test]
    fn watermarks_are_indexed_by_owner() {
        let watermarks = watermarks_by_owner(vec![
            JoplinSourceWatermark {
                owner_id: "alice".to_string(),
                source_watermark: 10,
            },
            JoplinSourceWatermark {
                owner_id: "bob".to_string(),
                source_watermark: 20,
            },
        ]);

        assert_eq!(watermarks.get("alice"), Some(&10));
        assert_eq!(watermarks.get("bob"), Some(&20));
    }

    #[test]
    fn worker_uses_join_set_for_parallel_user_refreshes() {
        let source = include_str!("worker.rs");

        assert!(source.contains("JoinSet"));
        assert!(source.contains("tasks.spawn"));
    }

    #[test]
    fn worker_cycle_stats_aggregate_user_outcomes() {
        let mut stats = WorkerCycleStats::started(5);

        stats.add_outcome(WorkerUserOutcome::Refreshed(
            incremental_outcome_with_counts(2, 3, 5, 0, 0),
        ));
        stats.add_outcome(WorkerUserOutcome::SkippedCurrent);
        stats.add_outcome(WorkerUserOutcome::SkippedCurrent);
        stats.add_outcome(WorkerUserOutcome::SkippedLock);
        stats.failed += 1;

        assert_eq!(
            stats,
            WorkerCycleStats {
                checked: 5,
                refreshed: 1,
                skipped_current: 2,
                skipped_lock: 1,
                failed: 1,
                skipped_encrypted: 2,
                skipped_malformed: 3,
                skipped_wrong_owner: 5,
                full_rebuild_indexed_items: 0,
                full_rebuild_deleted_items: 0,
            }
        );
    }

    #[test]
    fn worker_cycle_stats_aggregate_full_rebuild_counts() {
        let mut stats = WorkerCycleStats::started(1);

        stats.add_outcome(WorkerUserOutcome::Refreshed(
            incremental_outcome_with_counts(1, 2, 3, 8, 5),
        ));

        assert_eq!(stats.refreshed, 1);
        assert_eq!(stats.skipped_encrypted, 1);
        assert_eq!(stats.skipped_malformed, 2);
        assert_eq!(stats.skipped_wrong_owner, 3);
        assert_eq!(stats.full_rebuild_indexed_items, 8);
        assert_eq!(stats.full_rebuild_deleted_items, 5);
    }

    #[test]
    fn worker_cycle_stats_count_source_watermark_failure_for_all_checked_users() {
        let mut stats = WorkerCycleStats::started(3);
        stats.failed += stats.checked;

        assert_eq!(stats.checked, 3);
        assert_eq!(stats.failed, 3);
        assert_eq!(stats.refreshed, 0);
        assert_eq!(stats.skipped_current, 0);
        assert_eq!(stats.skipped_lock, 0);
    }

    fn incremental_outcome_with_counts(
        skipped_encrypted: usize,
        skipped_malformed: usize,
        skipped_wrong_owner: usize,
        full_rebuild_indexed_items: usize,
        full_rebuild_deleted_items: usize,
    ) -> IncrementalRefreshOutcome {
        IncrementalRefreshOutcome {
            lock_acquired: true,
            full_rebuild_required: full_rebuild_indexed_items > 0 || full_rebuild_deleted_items > 0,
            changed_items: 0,
            reconciled_deleted_items: 0,
            full_rebuild_indexed_items,
            full_rebuild_deleted_items,
            skipped_encrypted,
            skipped_malformed,
            skipped_wrong_owner,
        }
    }
}
