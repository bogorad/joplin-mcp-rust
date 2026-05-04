use anyhow::{Context, bail};

pub const SINGLETON_LOCK_KEY: i64 = 0x6a_6f_70_6c_69_6e_6d_63;

#[derive(Debug)]
pub struct SingletonLock {
    pool: sqlx::PgPool,
}

impl SingletonLock {
    pub async fn acquire(pool: &sqlx::PgPool) -> anyhow::Result<Self> {
        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(SINGLETON_LOCK_KEY)
            .fetch_one(pool)
            .await
            .context("acquire singleton advisory lock")?;

        if !acquired {
            bail!("another joplin-mcpd instance holds the singleton lock");
        }

        Ok(Self { pool: pool.clone() })
    }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(SINGLETON_LOCK_KEY)
                .execute(&pool)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn singleton_lock_key_is_stable() {
        assert_eq!(SINGLETON_LOCK_KEY, 0x6a_6f_70_6c_69_6e_6d_63);
    }
}
