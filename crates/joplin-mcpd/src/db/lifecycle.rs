use anyhow::{Context, bail};
use sqlx::PgConnection;

pub const SINGLETON_LOCK_KEY: i64 = 0x6a_6f_70_6c_69_6e_6d_63;

#[derive(Debug)]
pub struct SingletonLock {
    _conn: PgConnection,
}

impl SingletonLock {
    pub async fn acquire(pool: &sqlx::PgPool) -> anyhow::Result<Self> {
        let mut conn = pool
            .acquire()
            .await
            .context("acquire singleton lock database connection")?
            .detach();

        let acquired = sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(SINGLETON_LOCK_KEY)
            .fetch_one(&mut conn)
            .await
            .context("acquire singleton advisory lock")?;

        if !acquired {
            bail!("another joplin-mcpd instance holds the singleton lock");
        }

        Ok(Self { _conn: conn })
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
