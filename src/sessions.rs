use crate::tokens::hash;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sqlx::PgPool;

pub const TTL_DAYS: i64 = 30;

/// Returns the cookie value; only its hash is stored.
pub async fn create(pool: &PgPool, user_id: i64) -> sqlx::Result<String> {
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    sqlx::query(
        "INSERT INTO sessions (user_id, token_hash, expires_at)
         VALUES ($1, $2, now() + make_interval(days => $3))",
    )
    .bind(user_id)
    .bind(hash(&token))
    .bind(TTL_DAYS as i32)
    .execute(pool)
    .await?;
    Ok(token)
}

/// The session's user while unexpired. Sliding expiry: at most one write per hour.
pub async fn lookup(pool: &PgPool, cookie: &str) -> sqlx::Result<Option<i64>> {
    let row: Option<(i64, bool)> = sqlx::query_as(
        "SELECT user_id, last_seen_at < now() - interval '1 hour' FROM sessions
         WHERE token_hash = $1 AND expires_at > now()",
    )
    .bind(hash(cookie))
    .fetch_optional(pool)
    .await?;
    let Some((user, stale)) = row else {
        return Ok(None);
    };
    if stale {
        sqlx::query(
            "UPDATE sessions SET last_seen_at = now(),
                 expires_at = now() + make_interval(days => $2)
             WHERE token_hash = $1",
        )
        .bind(hash(cookie))
        .bind(TTL_DAYS as i32)
        .execute(pool)
        .await?;
    }
    Ok(Some(user))
}

pub async fn delete(pool: &PgPool, cookie: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(hash(cookie))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn purge_expired(pool: &PgPool) -> sqlx::Result<u64> {
    let r = sqlx::query("DELETE FROM sessions WHERE expires_at <= now()")
        .execute(pool)
        .await?;
    Ok(r.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    async fn expires(pool: &PgPool) -> chrono::DateTime<chrono::Utc> {
        sqlx::query_scalar("SELECT expires_at FROM sessions")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn create_lookup_delete() {
        let pool = test_pool().await;
        let c = create(&pool, 1).await.unwrap();
        assert!(c.len() >= 43);
        assert_eq!(lookup(&pool, &c).await.unwrap(), Some(1));
        assert_eq!(lookup(&pool, "garbage").await.unwrap(), None);
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(stored, c.as_bytes());
        delete(&pool, &c).await.unwrap();
        assert_eq!(lookup(&pool, &c).await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_is_none_and_purged() {
        let pool = test_pool().await;
        let old = create(&pool, 1).await.unwrap();
        let live = create(&pool, 1).await.unwrap();
        sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second' WHERE id = 1")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(lookup(&pool, &old).await.unwrap(), None);
        assert_eq!(purge_expired(&pool).await.unwrap(), 1);
        assert_eq!(lookup(&pool, &live).await.unwrap(), Some(1));
    }

    #[tokio::test]
    async fn touch_at_most_hourly() {
        let pool = test_pool().await;
        let c = create(&pool, 1).await.unwrap();
        let backdate = |ago: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE sessions SET last_seen_at = now() - interval '{ago}',
                         expires_at = now() + interval '1 day'"
                )))
                .execute(&pool)
                .await
                .unwrap();
            }
        };
        backdate("10 minutes").await;
        let before = expires(&pool).await;
        lookup(&pool, &c).await.unwrap();
        assert_eq!(expires(&pool).await, before);
        backdate("2 hours").await;
        lookup(&pool, &c).await.unwrap();
        assert!(expires(&pool).await > before + chrono::Duration::days(29));
    }
}
