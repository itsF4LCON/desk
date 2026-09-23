//! Three independent gates:
//! 1. Cloudflare Access JWT (`Cf-Access-Jwt-Assertion`) for the one allowed email.
//! 2. argon2id password.
//! 3. TOTP (RFC 6238, ±1 step, no reuse of an accepted step).
//!
//! Plus server-side sessions and a global lockout (single-user system).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use rand::RngExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::warn;

use crate::config::{AccessConfig, AuthState};

pub const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
pub const MAX_FAILURES: u32 = 5;
pub const LOCKOUT: Duration = Duration::from_secs(15 * 60);
const JWKS_MIN_REFRESH: Duration = Duration::from_secs(60);
const TOTP_STEP: u64 = 30;

// ---------------------------------------------------------------------------
// Cloudflare Access
// ---------------------------------------------------------------------------
#[derive(Debug, Deserialize)]
struct AccessClaims {
    email: Option<String>,
}

pub struct AccessVerifier {
    cfg: AccessConfig,
    certs_url: String,
    http: reqwest::Client,
    keys: RwLock<(JwkSet, Option<Instant>)>,
}

impl AccessVerifier {
    pub fn new(cfg: AccessConfig) -> Self {
        let certs_url = format!("https://{}/cdn-cgi/access/certs", cfg.team_domain);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("http client");
        Self {
            cfg,
            certs_url,
            http,
            keys: RwLock::new((JwkSet { keys: vec![] }, None)),
        }
    }

    #[cfg(test)]
    fn with_keys(cfg: AccessConfig, keys: JwkSet) -> Self {
        let v = Self::new(cfg);
        *v.keys.try_write().unwrap() = (keys, Some(Instant::now()));
        v
    }

    async fn refresh(&self) -> Result<()> {
        let mut guard = self.keys.write().await;
        if guard.1.is_some_and(|t| t.elapsed() < JWKS_MIN_REFRESH) {
            return Ok(());
        }
        let set: JwkSet = self
            .http
            .get(&self.certs_url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        *guard = (set, Some(Instant::now()));
        Ok(())
    }

    /// Returns the verified email, or an error (fail closed on any problem, including JWKS fetch).
    pub async fn verify(&self, token: &str) -> Result<String> {
        let header = jsonwebtoken::decode_header(token).context("malformed Access token")?;
        if header.alg != Algorithm::RS256 {
            bail!("unexpected Access token algorithm");
        }
        let kid = header
            .kid
            .ok_or_else(|| anyhow!("Access token has no kid"))?;

        let mut key = self.keys.read().await.0.find(&kid).cloned();
        if key.is_none() {
            self.refresh()
                .await
                .context("cannot fetch Access signing keys")?;
            key = self.keys.read().await.0.find(&kid).cloned();
        }
        let jwk = key.ok_or_else(|| anyhow!("unknown Access signing key"))?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[&self.cfg.aud]);
        validation.set_issuer(&[format!("https://{}", self.cfg.team_domain)]);
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        let data =
            jsonwebtoken::decode::<AccessClaims>(token, &DecodingKey::from_jwk(&jwk)?, &validation)
                .context("Access token rejected")?;

        let email = data.claims.email.unwrap_or_default();
        if !email.eq_ignore_ascii_case(&self.cfg.allowed_email) {
            bail!("Access identity {email:?} is not allowed");
        }
        Ok(email)
    }
}

// ---------------------------------------------------------------------------
// Password + TOTP
// ---------------------------------------------------------------------------
pub fn hash_password(password: &str) -> Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|h| h.to_string())
        .map_err(|e| anyhow!("hashing failed: {e}"))
}

pub fn verify_password(hash: &str, password: &str) -> bool {
    // Parameters come from the PHC string itself.
    PasswordVerifier::<str>::verify_password(&Argon2::default(), password.as_bytes(), hash).is_ok()
}

pub fn totp(secret_b32: &str, issuer: &str, account: &str) -> Result<totp_rs::Totp> {
    let secret = totp_rs::Secret::try_from_base32(secret_b32)
        .map_err(|e| anyhow!("bad TOTP secret: {e:?}"))?;
    totp_rs::Builder::new()
        .with_secret(secret)
        .with_skew(1)
        .with_step_duration(TOTP_STEP)
        .with_issuer(Some(issuer))
        .with_account_name(account)
        .build()
        .map_err(|e| anyhow!("TOTP setup failed: {e:?}"))
}

pub fn new_totp_secret() -> String {
    let bytes: [u8; 20] = rand::rng().random();
    totp_rs::Secret::from(bytes.to_vec()).to_base32()
}

/// Returns the accepted time step, enforcing single use of each step.
pub fn check_totp(state: &AuthState, code: &str, now_unix: u64) -> Option<u64> {
    let totp = totp(&state.totp_secret, "desk", "owner").ok()?;
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let step = totp.check(&code, now_unix)?;
    (step > state.last_totp_step).then_some(step)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Lockout + sessions
// ---------------------------------------------------------------------------
#[derive(Default)]
struct Lockout {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Default)]
pub struct Sessions {
    // sha256(token) → expiry. Only hashes are kept in memory.
    inner: Mutex<HashMap<[u8; 32], Instant>>,
    lockout: Mutex<Lockout>,
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl Sessions {
    /// Remaining lockout, if any.
    pub fn locked_for(&self, now: Instant) -> Option<Duration> {
        let l = self.lockout.lock().unwrap();
        l.locked_until
            .and_then(|t| t.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    pub fn record_failure(&self, now: Instant) {
        let mut l = self.lockout.lock().unwrap();
        l.failures += 1;
        if l.failures >= MAX_FAILURES {
            l.failures = 0;
            l.locked_until = Some(now + LOCKOUT);
            warn!(
                "too many failed logins; locked for {} minutes",
                LOCKOUT.as_secs() / 60
            );
        }
    }

    pub fn record_success(&self) {
        *self.lockout.lock().unwrap() = Lockout::default();
    }

    /// Creates a session and returns its bearer token (only ever sent in the cookie).
    pub fn create(&self, now: Instant) -> String {
        let bytes: [u8; 32] = rand::rng().random();
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let mut map = self.inner.lock().unwrap();
        map.retain(|_, exp| *exp > now);
        map.insert(token_hash(&token), now + SESSION_TTL);
        token
    }

    pub fn is_valid(&self, token: &str, now: Instant) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(&token_hash(token))
            .is_some_and(|exp| *exp > now)
    }

    pub fn revoke(&self, token: &str) {
        self.inner.lock().unwrap().remove(&token_hash(token));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;

    fn cfg() -> AccessConfig {
        AccessConfig {
            team_domain: "example.cloudflareaccess.com".into(),
            aud: "aud-tag-123".into(),
            allowed_email: "owner@example.com".into(),
        }
    }

    fn verifier() -> AccessVerifier {
        let jwks: JwkSet =
            serde_json::from_str(include_str!("../tests/fixtures/test_jwks.json")).unwrap();
        AccessVerifier::with_keys(cfg(), jwks)
    }

    fn token(claims: serde_json::Value, kid: &str) -> String {
        let key =
            EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/test_rsa.pem")).unwrap();
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.into());
        jsonwebtoken::encode(&header, &claims, &key).unwrap()
    }

    fn claims(email: &str, aud: &str, iss: &str, exp_offset: i64) -> serde_json::Value {
        json!({ "email": email, "aud": [aud], "iss": iss, "exp": unix_now() as i64 + exp_offset })
    }

    const ISS: &str = "https://example.cloudflareaccess.com";

    #[tokio::test]
    async fn access_accepts_valid_token() {
        let t = token(
            claims("Owner@Example.com", "aud-tag-123", ISS, 300),
            "test-kid",
        );
        assert_eq!(verifier().verify(&t).await.unwrap(), "Owner@Example.com");
    }

    #[tokio::test]
    async fn access_rejects_bad_tokens() {
        let v = verifier();
        for (why, t) in [
            (
                "wrong email",
                token(
                    claims("evil@gmail.com", "aud-tag-123", ISS, 300),
                    "test-kid",
                ),
            ),
            (
                "wrong aud",
                token(claims("owner@example.com", "other", ISS, 300), "test-kid"),
            ),
            (
                "wrong iss",
                token(
                    claims(
                        "owner@example.com",
                        "aud-tag-123",
                        "https://evil.cloudflareaccess.com",
                        300,
                    ),
                    "test-kid",
                ),
            ),
            (
                "expired",
                token(
                    claims("owner@example.com", "aud-tag-123", ISS, -600),
                    "test-kid",
                ),
            ),
        ] {
            assert!(v.verify(&t).await.is_err(), "{why} should be rejected");
        }
        // Tampered payload: signature no longer matches.
        let good = token(
            claims("owner@example.com", "aud-tag-123", ISS, 300),
            "test-kid",
        );
        let mut parts: Vec<&str> = good.split('.').collect();
        let forged = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(claims("owner@example.com", "aud-tag-123", ISS, 99999).to_string());
        parts[1] = &forged;
        assert!(v.verify(&parts.join(".")).await.is_err(), "forged payload");
        assert!(v.verify("not-a-jwt").await.is_err());
    }

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password(&h, "correct horse"));
        assert!(!verify_password(&h, "correct hors"));
        assert!(!verify_password("garbage", "x"));
    }

    #[test]
    fn totp_window_and_replay() {
        let secret = new_totp_secret();
        let t = totp(&secret, "desk", "owner").unwrap();
        let now = 1_800_000_000u64;
        let mut state = AuthState {
            password_hash: String::new(),
            totp_secret: secret,
            last_totp_step: 0,
        };
        let code = t.generate(now).to_string();
        let step = check_totp(&state, &code, now).expect("current code accepted");
        assert_eq!(step, now / TOTP_STEP);
        // One step of skew is fine, three is not.
        assert!(check_totp(&state, &t.generate(now - 30).to_string(), now).is_some());
        assert!(check_totp(&state, &t.generate(now - 90).to_string(), now).is_none());
        // Replay of an accepted step is refused.
        state.last_totp_step = step;
        assert!(check_totp(&state, &code, now).is_none());
        assert!(check_totp(&state, "12345", now).is_none());
    }

    #[test]
    fn sessions_and_lockout() {
        let s = Sessions::default();
        let now = Instant::now();
        let tok = s.create(now);
        assert!(s.is_valid(&tok, now));
        assert!(!s.is_valid(&tok, now + SESSION_TTL + Duration::from_secs(1)));
        assert!(!s.is_valid("forged", now));
        s.revoke(&tok);
        assert!(!s.is_valid(&tok, now));

        for _ in 0..MAX_FAILURES - 1 {
            s.record_failure(now);
        }
        assert!(s.locked_for(now).is_none());
        s.record_failure(now);
        assert!(s.locked_for(now).is_some());
        assert!(
            s.locked_for(now + LOCKOUT + Duration::from_secs(1))
                .is_none()
        );
    }
}
