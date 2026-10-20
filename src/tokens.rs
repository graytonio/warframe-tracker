use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

pub const MAX_TOKENS: i64 = 10;

pub(crate) fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[derive(Debug)]
pub enum CreateError {
    /// A message safe to show the user.
    Invalid(String),
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CreateError {
    fn from(e: sqlx::Error) -> Self {
        CreateError::Db(e)
    }
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CreateError::Invalid(m) => f.write_str(m),
            CreateError::Db(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for CreateError {}

/// Returns the plaintext token; only its hash is stored. Names are unique per user
/// among live tokens, and a user has at most `MAX_TOKENS` live tokens.
pub async fn create(pool: &PgPool, user_id: i64, name: &str) -> Result<String, CreateError> {
    if name.is_empty() || name.chars().count() > 64 {
        return Err(CreateError::Invalid(
            "token name must be 1-64 characters".into(),
        ));
    }
    // ponytail: count-then-insert can race past the limit by a token or two; fine for a cap.
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM api_tokens WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;
    if live >= MAX_TOKENS {
        return Err(CreateError::Invalid(format!(
            "at most {MAX_TOKENS} tokens; revoke one first"
        )));
    }
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let r = sqlx::query("INSERT INTO api_tokens (user_id, name, token_hash) VALUES ($1, $2, $3)")
        .bind(user_id)
        .bind(name)
        .bind(hash(&token))
        .execute(pool)
        .await;
    match r {
        Ok(_) => Ok(token),
        Err(sqlx::Error::Database(d)) if d.constraint() == Some("api_tokens_user_name") => Err(
            CreateError::Invalid(format!("a token named {name} already exists")),
        ),
        Err(e) => Err(e.into()),
    }
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

/// `false` when the user has no live token with that name.
pub async fn revoke(pool: &PgPool, user_id: i64, name: &str) -> sqlx::Result<bool> {
    let r = sqlx::query(
        "UPDATE api_tokens SET revoked_at = now()
         WHERE user_id = $1 AND name = $2 AND revoked_at IS NULL",
    )
    .bind(user_id)
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
        assert!(!super::revoke(&pool, 2, "pc").await.unwrap()); // not user 2's
        assert!(super::revoke(&pool, 1, "pc").await.unwrap());
        assert!(!super::revoke(&pool, 1, "pc").await.unwrap());
        assert_eq!(super::authenticate(&pool, &t).await.unwrap(), None);
        let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM api_tokens")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(stored, t.as_bytes()); // only the hash is stored
        // A revoked name can be reused.
        super::create(&pool, 1, "pc").await.unwrap();
    }
}
