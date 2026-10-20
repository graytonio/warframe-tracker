use sqlx::PgPool;
use std::time::Duration;

/// The Zalando DB can come up after the pod, so wait for it rather than crash-looping.
pub async fn connect_with_retry(url: &str) -> PgPool {
    let mut delay = Duration::from_millis(500);
    loop {
        match PgPool::connect(url).await {
            Ok(p) => return p,
            Err(e) => {
                eprintln!("db connect failed ({e}); retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

pub async fn migrate(pool: &PgPool) -> sqlx::Result<()> {
    sqlx::raw_sql(include_str!("../schema.sql"))
        .execute(pool)
        .await?;
    Ok(())
}

/// Fresh, migrated database per call so tests (and parallel worktrees) never share state.
// ponytail: test databases are never dropped; `docker compose down -v` clears them.
#[cfg(test)]
pub async fn test_pool() -> PgPool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let admin = PgPool::connect(&base).await.unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!(
        "t_{}_{}_{}",
        std::process::id(),
        nanos,
        N.fetch_add(1, Ordering::Relaxed)
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = base.rsplit_once('/').unwrap();
    let pool = PgPool::connect(&format!("{prefix}/{name}")).await.unwrap();
    migrate(&pool).await.unwrap();
    pool
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn migrate_is_idempotent_and_seeds_default_user() {
        let pool = super::test_pool().await;
        super::migrate(&pool).await.unwrap();
        let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(name, "default");
    }
}
