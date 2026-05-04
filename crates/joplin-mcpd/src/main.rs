use anyhow::Context;
use clap::Parser;
use joplin_mcpd::{
    auth::tokens::{TokenRepository, load_hmac_keys},
    config::Config,
    db::{lifecycle::SingletonLock, migrations, schema_check::validate_joplin_source_schema},
    http::{ApiBackend, serve_with_backends},
    indexer::{source::JoplinDbSource, worker::spawn_index_worker},
    lifecycle::{Readiness, ReadinessStatus},
    logging::init_logging,
    mcp::transport::McpAuth,
    observability::metrics,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long, env = "JP_MCPD_CONFIG")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = Config::read(args.config.as_deref()).context("read config")?;
    let log_guard = init_logging(&config.logging).context("initialize logging")?;
    config.validate().context("validate config")?;

    let mcp_pool = connect_mcp_pool(&config).await?;
    migrations::run(&mcp_pool)
        .await
        .context("run joplin_mcp migrations")?;
    let singleton_lock = SingletonLock::acquire(&mcp_pool).await?;
    let joplin_pool = connect_joplin_pool(&config).await?;
    validate_joplin_source_schema(&joplin_pool)
        .await
        .context("validate Joplin source schema")?;
    let readiness = Readiness::new(probe_joplin_readiness(&config).await);
    let hmac_keys = Arc::new(load_hmac_keys(&config.tokens.hmac_keys)?);
    let active_hmac_key = hmac_keys
        .iter()
        .find(|key| key.id == config.tokens.active_hmac_key_id)
        .cloned()
        .context("active HMAC key is missing")?;
    let token_repository = TokenRepository::new(mcp_pool.clone());
    let mcp_auth = McpAuth::Repository {
        repository: token_repository.clone(),
        hmac_keys: hmac_keys.clone(),
    };
    let api_backend = ApiBackend::Repository {
        mcp_pool: mcp_pool.clone(),
        joplin_pool: joplin_pool.clone(),
        token_repository,
        hmac_keys,
        active_hmac_key,
    };
    let index_worker = spawn_index_worker(
        mcp_pool.clone(),
        JoplinDbSource::new(joplin_pool.clone()),
        config.index.clone(),
    );
    serve_with_backends(config, readiness, mcp_auth, api_backend).await?;
    index_worker.abort();
    let _ = index_worker.await;
    drop(singleton_lock);
    joplin_pool.close().await;
    mcp_pool.close().await;
    log_guard.flush();
    Ok(())
}

async fn probe_joplin_readiness(config: &Config) -> ReadinessStatus {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(config.server.request_timeout_seconds))
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            tracing::warn!(operation = "joplin_readiness_probe", %error, "failed to build HTTP client");
            return ReadinessStatus::NotReady;
        }
    };

    match client.get(config.joplin.base_url.clone()).send().await {
        Ok(_) => ReadinessStatus::Ready,
        Err(error) => {
            tracing::warn!(operation = "joplin_readiness_probe", %error, "Joplin auth endpoint is temporarily unreachable");
            ReadinessStatus::NotReady
        }
    }
}

async fn connect_mcp_pool(config: &Config) -> anyhow::Result<sqlx::PgPool> {
    let dsn_file = config
        .postgres
        .mcp_dsn_file
        .as_deref()
        .context("postgres.mcp_dsn_file is required")?;
    let dsn = fs::read_to_string(dsn_file).context("read postgres.mcp_dsn_file credential")?;
    let started = std::time::Instant::now();
    let pool = PgPoolOptions::new()
        .max_connections(config.postgres.runtime_max_connections)
        .acquire_timeout(Duration::from_secs(config.postgres.acquire_timeout_seconds))
        .connect_with(postgres_connect_options(
            dsn.trim(),
            config.postgres.statement_timeout_seconds,
        )?)
        .await
        .context("connect to MCP database")?;
    metrics::record_postgres_pool_wait("runtime", started.elapsed());
    Ok(pool)
}

async fn connect_joplin_pool(config: &Config) -> anyhow::Result<sqlx::PgPool> {
    let dsn_file = config
        .postgres
        .joplin_dsn_file
        .as_deref()
        .context("postgres.joplin_dsn_file is required")?;
    let dsn = fs::read_to_string(dsn_file).context("read postgres.joplin_dsn_file credential")?;
    let started = std::time::Instant::now();
    let pool = PgPoolOptions::new()
        .max_connections(config.postgres.indexer_max_connections)
        .acquire_timeout(Duration::from_secs(config.postgres.acquire_timeout_seconds))
        .connect_with(postgres_connect_options(
            dsn.trim(),
            config.postgres.statement_timeout_seconds,
        )?)
        .await
        .context("connect to Joplin source database")?;
    metrics::record_postgres_pool_wait("indexer", started.elapsed());
    Ok(pool)
}

fn postgres_connect_options(
    dsn: &str,
    statement_timeout_seconds: u64,
) -> anyhow::Result<PgConnectOptions> {
    let statement_timeout_millis = statement_timeout_seconds
        .checked_mul(1000)
        .context("postgres.statement_timeout_seconds is too large")?;
    Ok(dsn
        .parse::<PgConnectOptions>()
        .context("parse postgres DSN credential")?
        .options([("statement_timeout", statement_timeout_millis)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_connect_options_applies_statement_timeout_in_milliseconds() {
        let options = postgres_connect_options("postgres://mcp_user:secret@127.0.0.1/mcp_db", 15)
            .expect("connect options are built");

        assert_eq!(options.get_options(), Some("-c statement_timeout=15000"));
    }

    #[test]
    fn postgres_connect_options_preserves_existing_startup_options() {
        let options = postgres_connect_options(
            "postgres://joplin_user:secret@127.0.0.1/joplin_db?options=-c%20geqo=off",
            2,
        )
        .expect("connect options are built");

        assert_eq!(
            options.get_options(),
            Some("-c geqo=off -c statement_timeout=2000")
        );
    }
}
