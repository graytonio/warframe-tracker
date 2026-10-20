use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub(crate) fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Returns the plaintext token; only its hash is stored.
pub async fn create(pool: &PgPool, user_id: i64, name: &str) -> sqlx::Result<String> {
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    sqlx::query("INSERT INTO api_tokens (user_id, name, token_hash) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(name)
        .bind(hash(&token))
        .execute(pool)
        .await?;
    Ok(token)
}

/// The token's user, if it is valid and not revoked. Bumps `last_used_at`.
pub async fn authenticate(pool: &PgPool, token: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "UPDATE api_tokens SET last_used_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL RETURNING user_id",
    )
    .bind(hash(token))
    .fetch_optional(pool)
    .await
}

/// `false` when there is no live token with that name.
pub async fn revoke(pool: &PgPool, name: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now() WHERE name = $1 AND revoked_at IS NULL",
    )
    .bind(name)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn create_authenticate_revoke() {
        let pool = crate::db::test_pool().await;
        let t = super::create(&pool, 1, "pc").await.unwrap();
        assert!(t.len() >= 43); // 32 bytes base64url
        assert_eq!(super::authenticate(&pool, &t).await.unwrap(), Some(1));
        assert_eq!(super::authenticate(&pool, "garbage").await.unwrap(), None);
        assert!(super::revoke(&pool, "pc").await.unwrap());
        assert!(!super::revoke(&pool, "pc").await.unwrap());
        assert_eq!(super::authenticate(&pool, &t).await.unwrap(), None);
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(stored, t.as_bytes()); // only the hash is stored
    }
}
