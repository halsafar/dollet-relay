//! Access and refresh tokens.
//!
//! Claim names are `user_id` and `exp` rather than the JWT-standard `sub`, because
//! the SPA reads `user_id` and `exp` straight out of the stored access token.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64URL;
use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use uuid::Uuid;

use crate::domain::Id;
use crate::{Error, Result};

const ACCESS_TTL_MINUTES: i64 = 30;
const REFRESH_TTL_DAYS: i64 = 1;

pub const ACCESS: &str = "access";
pub const REFRESH: &str = "refresh";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub token_type: String,
    pub exp: i64,
    pub iat: i64,
    pub jti: String,
    pub user_id: Id,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenPair {
    pub access: String,
    pub refresh: String,
}

#[derive(Clone)]
pub struct Jwt {
    encoding: EncodingKey,
    decoding: DecodingKey,
}

impl std::fmt::Debug for Jwt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Jwt(<redacted>)")
    }
}

impl Jwt {
    pub fn new(secret: &[u8]) -> Self {
        Self {
            encoding: EncodingKey::from_secret(secret),
            decoding: DecodingKey::from_secret(secret),
        }
    }

    /// Read the instance signing key, generating one on first start.
    ///
    /// Persisted rather than generated per process so a restart does not log
    /// every client out — and kept out of `core_setting` so no settings
    /// endpoint can ever serve it.
    pub async fn load_or_create(pool: &SqlitePool) -> Result<Self> {
        let existing: Option<String> =
            sqlx::query_scalar("SELECT jwt_secret FROM instance_secret WHERE id = 1")
                .fetch_optional(pool)
                .await?;

        if let Some(secret) = existing {
            return Ok(Self::new(secret.as_bytes()));
        }

        let secret = random_token(48);
        // Two workers racing on first start must end up with the same key, or
        // tokens issued by one are rejected by the other.
        sqlx::query("INSERT OR IGNORE INTO instance_secret (id, jwt_secret) VALUES (1, ?)")
            .bind(&secret)
            .execute(pool)
            .await?;

        let stored: String =
            sqlx::query_scalar("SELECT jwt_secret FROM instance_secret WHERE id = 1")
                .fetch_one(pool)
                .await?;
        Ok(Self::new(stored.as_bytes()))
    }

    pub fn issue_pair(&self, user_id: Id) -> Result<TokenPair> {
        Ok(TokenPair {
            access: self.issue(user_id, ACCESS, Duration::minutes(ACCESS_TTL_MINUTES))?,
            refresh: self.issue(user_id, REFRESH, Duration::days(REFRESH_TTL_DAYS))?,
        })
    }

    pub fn issue(&self, user_id: Id, token_type: &str, ttl: Duration) -> Result<String> {
        let now = Utc::now();
        let claims = Claims {
            token_type: token_type.to_owned(),
            exp: (now + ttl).timestamp(),
            iat: now.timestamp(),
            jti: Uuid::new_v4().simple().to_string(),
            user_id,
        };

        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &self.encoding)
            .map_err(|e| Error::Other(e.into()))
    }

    /// Decode and check expiry, then check the token is the kind being asked
    /// for. Without the second half a refresh token — which lives 48x longer —
    /// would work as a bearer token.
    pub fn verify(&self, token: &str, expected_type: &str) -> Result<Claims> {
        // `exp` stays required: the library refuses a token without one, which
        // is the guard against a forged claim set that simply omits expiry.
        let validation = Validation::new(Algorithm::HS256);

        let data = jsonwebtoken::decode::<Claims>(token, &self.decoding, &validation)
            .map_err(|_| Error::Unauthorized)?;

        if data.claims.token_type != expected_type {
            return Err(Error::Unauthorized);
        }
        Ok(data.claims)
    }

    /// Exchange a refresh token for a new access token. Refresh tokens are not
    /// rotated, so the client keeps the one it has.
    pub fn refresh(&self, refresh_token: &str) -> Result<String> {
        let claims = self.verify(refresh_token, REFRESH)?;
        self.issue(
            claims.user_id,
            ACCESS,
            Duration::minutes(ACCESS_TTL_MINUTES),
        )
    }
}

/// URL-safe random string, used for signing keys and API keys.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rng().fill(&mut buf[..]);
    BASE64URL.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt() -> Jwt {
        Jwt::new(b"test-secret")
    }

    #[test]
    fn issues_a_pair_that_verifies() {
        let pair = jwt().issue_pair(7).unwrap();
        assert_eq!(jwt().verify(&pair.access, ACCESS).unwrap().user_id, 7);
        assert_eq!(jwt().verify(&pair.refresh, REFRESH).unwrap().user_id, 7);
    }

    #[test]
    fn a_refresh_token_is_not_a_bearer_token() {
        let pair = jwt().issue_pair(1).unwrap();
        assert!(jwt().verify(&pair.refresh, ACCESS).is_err());
        assert!(jwt().verify(&pair.access, REFRESH).is_err());
    }

    #[test]
    fn another_instances_tokens_are_rejected() {
        let pair = jwt().issue_pair(1).unwrap();
        assert!(Jwt::new(b"different").verify(&pair.access, ACCESS).is_err());
    }

    #[test]
    fn expired_tokens_are_rejected() {
        let token = jwt().issue(1, ACCESS, Duration::minutes(-10)).unwrap();
        assert!(jwt().verify(&token, ACCESS).is_err());
    }

    #[test]
    fn refresh_yields_a_usable_access_token() {
        let pair = jwt().issue_pair(42).unwrap();
        let access = jwt().refresh(&pair.refresh).unwrap();
        assert_eq!(jwt().verify(&access, ACCESS).unwrap().user_id, 42);

        // An access token must not be usable to mint more access tokens.
        assert!(jwt().refresh(&pair.access).is_err());
    }

    #[test]
    fn random_tokens_are_url_safe_and_unique() {
        let a = random_token(32);
        let b = random_token(32);
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[tokio::test]
    async fn the_signing_key_survives_a_restart() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        crate::db::MIGRATOR.run(&pool).await.unwrap();

        let first = Jwt::load_or_create(&pool).await.unwrap();
        let token = first.issue(3, ACCESS, Duration::minutes(5)).unwrap();

        let second = Jwt::load_or_create(&pool).await.unwrap();
        assert_eq!(second.verify(&token, ACCESS).unwrap().user_id, 3);
    }
}
