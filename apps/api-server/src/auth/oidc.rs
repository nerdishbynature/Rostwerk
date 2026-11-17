//! Verification of OIDC / OAuth 2 JWT access tokens.
//!
//! Signing keys come from the issuer's JWKS, found via
//! `<issuer>/.well-known/openid-configuration`. Keys are cached; an unknown
//! `kid` (key rotation) triggers a refetch, rate-limited so that garbage
//! tokens cannot make us hammer the issuer.
//!
//! Checks: signature with a key from the JWKS, `alg` must match that key
//! (no `none`, no HMAC, no algorithm confusion), `iss`, `aud`, `exp` and `nbf`
//! with a small leeway, and a non-empty `sub`.

use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{AlgorithmParameters, EllipticCurve, Jwk, KeyAlgorithm, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, OnceCell, RwLock};

/// Clock skew tolerated for `exp` and `nbf`.
const LEEWAY_SECS: u64 = 30;
/// Default: refetch the JWKS at most this often when a token names an unknown key.
const MIN_REFRESH: Duration = Duration::from_secs(30);
/// Refetch the JWKS after this long even if all keys are known (drops retired keys).
const MAX_AGE: Duration = Duration::from_secs(60 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// The claims Rostwerk uses. Everything else in the token is ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Claims {
    /// Stable identifier of the user or client at the issuer.
    pub sub: String,
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("malformed token: {0}")]
    Malformed(jsonwebtoken::errors::Error),
    #[error("no signing key for kid {0:?}")]
    UnknownKey(Option<String>),
    #[error("invalid token: {0}")]
    Invalid(jsonwebtoken::errors::Error),
    #[error("token has no subject")]
    NoSubject,
    #[error("cannot load signing keys: {0}")]
    Keys(String),
}

struct VerifyKey {
    kid: Option<String>,
    alg: Algorithm,
    key: DecodingKey,
}

#[derive(Default)]
struct KeyCache {
    keys: Vec<VerifyKey>,
    fetched_at: Option<Instant>,
}

impl KeyCache {
    fn find(&self, kid: Option<&str>) -> Option<&VerifyKey> {
        match kid {
            Some(kid) => self.keys.iter().find(|k| k.kid.as_deref() == Some(kid)),
            // Without a kid, only an unambiguous key set will do.
            None if self.keys.len() == 1 => self.keys.first(),
            None => None,
        }
    }

    fn may_refresh(&self, min_refresh: Duration) -> bool {
        self.fetched_at.is_none_or(|at| at.elapsed() >= min_refresh)
    }

    fn is_stale(&self) -> bool {
        self.fetched_at.is_none_or(|at| at.elapsed() >= MAX_AGE)
    }
}

enum KeySource {
    /// Discover the JWKS from the issuer and fetch it over HTTP.
    Discovery {
        http: reqwest::Client,
        jwks_uri: OnceCell<String>,
    },
    /// Fixed keys, for tests.
    Static,
}

/// How Swagger UI signs in: authorization code flow with PKCE, public client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsLogin {
    pub client_id: String,
    pub authorization_url: String,
    pub token_url: String,
}

/// Verifies access tokens of one issuer for one audience.
pub struct OidcVerifier {
    issuer: String,
    audience: String,
    min_refresh: Duration,
    source: KeySource,
    cache: RwLock<KeyCache>,
    /// Serialises refetches, so concurrent requests with a new kid fetch once.
    refresh: Mutex<()>,
}

impl std::fmt::Debug for OidcVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcVerifier")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .finish_non_exhaustive()
    }
}

impl OidcVerifier {
    /// Verifier that discovers the issuer's keys on first use.
    pub fn discover(
        issuer: impl Into<String>,
        audience: impl Into<String>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?;
        Ok(Self::new(
            issuer.into(),
            audience.into(),
            KeySource::Discovery {
                http,
                jwks_uri: OnceCell::new(),
            },
            KeyCache::default(),
        ))
    }

    /// Verifier with a fixed JWKS document (`{"keys": [...]}`) and no network access.
    pub fn with_jwks(issuer: impl Into<String>, audience: impl Into<String>, jwks: &Value) -> Self {
        let cache = KeyCache {
            keys: parse_jwks(jwks),
            fetched_at: Some(Instant::now()),
        };
        Self::new(issuer.into(), audience.into(), KeySource::Static, cache)
    }

    fn new(issuer: String, audience: String, source: KeySource, cache: KeyCache) -> Self {
        Self {
            issuer,
            audience,
            min_refresh: MIN_REFRESH,
            source,
            cache: RwLock::new(cache),
            refresh: Mutex::new(()),
        }
    }

    /// Changes how often an unknown `kid` may trigger a refetch (default 30 s).
    pub fn with_min_refresh(mut self, min_refresh: Duration) -> Self {
        self.min_refresh = min_refresh;
        self
    }

    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// URL of the issuer's OpenID Connect discovery document.
    pub fn discovery_url(&self) -> String {
        format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        )
    }

    /// Looks up the issuer's authorization and token endpoints for the
    /// browser sign-in of `/docs`. Needs network access to the issuer.
    pub async fn docs_login(&self, client_id: &str) -> Result<DocsLogin, VerifyError> {
        let KeySource::Discovery { http, .. } = &self.source else {
            return Err(VerifyError::Keys("no discovery for static keys".into()));
        };
        let doc = discover(http, &self.issuer, &self.discovery_url()).await?;
        match (doc.authorization_endpoint, doc.token_endpoint) {
            (Some(authorization_url), Some(token_url)) => Ok(DocsLogin {
                client_id: client_id.to_string(),
                authorization_url,
                token_url,
            }),
            _ => Err(VerifyError::Keys(
                "discovery document lacks authorization_endpoint or token_endpoint".into(),
            )),
        }
    }

    pub async fn verify(&self, token: &str) -> Result<Claims, VerifyError> {
        let header = jsonwebtoken::decode_header(token).map_err(VerifyError::Malformed)?;
        let kid = header.kid.as_deref();

        if self.cache.read().await.is_stale() {
            // Best effort: on failure keep verifying with the keys we have.
            if let Err(err) = self.refresh(false).await {
                tracing::warn!(error = %err, issuer = %self.issuer, "JWKS refresh failed");
            }
        }
        if self.cache.read().await.find(kid).is_none() {
            self.refresh(true).await?;
        }

        let cache = self.cache.read().await;
        let key = cache
            .find(kid)
            .ok_or_else(|| VerifyError::UnknownKey(header.kid.clone()))?;

        // The key decides the algorithm, never the token header.
        let mut validation = Validation::new(key.alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECS;

        let data = jsonwebtoken::decode::<Claims>(token, &key.key, &validation)
            .map_err(VerifyError::Invalid)?;
        if data.claims.sub.is_empty() {
            return Err(VerifyError::NoSubject);
        }
        Ok(data.claims)
    }

    /// Refetches the JWKS. With `for_unknown_kid`, respects `min_refresh`.
    async fn refresh(&self, for_unknown_kid: bool) -> Result<(), VerifyError> {
        let KeySource::Discovery { http, jwks_uri } = &self.source else {
            return Ok(());
        };
        let _guard = self.refresh.lock().await;
        {
            // Another request may have refreshed while we waited for the lock.
            let cache = self.cache.read().await;
            let due = if for_unknown_kid {
                cache.may_refresh(self.min_refresh)
            } else {
                cache.is_stale()
            };
            if !due {
                return Ok(());
            }
        }

        let result = async {
            let uri = jwks_uri
                .get_or_try_init(|| async {
                    discover(http, &self.issuer, &self.discovery_url())
                        .await
                        .map(|doc| doc.jwks_uri)
                })
                .await?;
            fetch_json(http, uri).await.map(|jwks| parse_jwks(&jwks))
        }
        .await;

        let mut cache = self.cache.write().await;
        // Count failures too, so an unreachable issuer is not retried on every request.
        cache.fetched_at = Some(Instant::now());
        let keys = result?;
        tracing::debug!(issuer = %self.issuer, keys = keys.len(), "JWKS loaded");
        cache.keys = keys;
        Ok(())
    }
}

async fn fetch_json(http: &reqwest::Client, url: &str) -> Result<Value, VerifyError> {
    let res = http
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| VerifyError::Keys(format!("GET {url}: {e}")))?;
    res.json()
        .await
        .map_err(|e| VerifyError::Keys(format!("GET {url}: {e}")))
}

/// The parts of the OpenID Connect discovery document Rostwerk uses.
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
}

async fn discover(
    http: &reqwest::Client,
    issuer: &str,
    discovery_url: &str,
) -> Result<Discovery, VerifyError> {
    let doc: Discovery = serde_json::from_value(fetch_json(http, discovery_url).await?)
        .map_err(|e| VerifyError::Keys(format!("discovery document: {e}")))?;
    // OpenID Connect Discovery 1.0, section 4.3: the issuer must match exactly.
    if doc.issuer != issuer {
        return Err(VerifyError::Keys(format!(
            "discovery document is for issuer {:?}, expected {issuer:?}",
            doc.issuer
        )));
    }
    Ok(doc)
}

/// Extracts the usable signing keys. Keys that cannot verify signatures
/// (encryption keys, symmetric keys, unknown types) are skipped, not fatal.
fn parse_jwks(jwks: &Value) -> Vec<VerifyKey> {
    let Some(keys) = jwks.get("keys").and_then(Value::as_array) else {
        return Vec::new();
    };
    keys.iter()
        .filter_map(|raw| {
            let jwk: Jwk = serde_json::from_value(raw.clone()).ok()?;
            if matches!(jwk.common.public_key_use, Some(ref u) if *u != PublicKeyUse::Signature) {
                return None;
            }
            let alg = signing_algorithm(&jwk)?;
            let key = DecodingKey::from_jwk(&jwk).ok()?;
            Some(VerifyKey {
                kid: jwk.common.key_id.clone(),
                alg,
                key,
            })
        })
        .collect()
}

/// The asymmetric signature algorithm a key is for: its `alg`, or the
/// default for its key type when `alg` is absent.
fn signing_algorithm(jwk: &Jwk) -> Option<Algorithm> {
    if let Some(alg) = jwk.common.key_algorithm {
        return match alg {
            KeyAlgorithm::RS256 => Some(Algorithm::RS256),
            KeyAlgorithm::RS384 => Some(Algorithm::RS384),
            KeyAlgorithm::RS512 => Some(Algorithm::RS512),
            KeyAlgorithm::PS256 => Some(Algorithm::PS256),
            KeyAlgorithm::PS384 => Some(Algorithm::PS384),
            KeyAlgorithm::PS512 => Some(Algorithm::PS512),
            KeyAlgorithm::ES256 => Some(Algorithm::ES256),
            KeyAlgorithm::ES384 => Some(Algorithm::ES384),
            KeyAlgorithm::EdDSA => Some(Algorithm::EdDSA),
            _ => None,
        };
    }
    match &jwk.algorithm {
        AlgorithmParameters::RSA(_) => Some(Algorithm::RS256),
        AlgorithmParameters::EllipticCurve(ec) => match ec.curve {
            EllipticCurve::P256 => Some(Algorithm::ES256),
            EllipticCurve::P384 => Some(Algorithm::ES384),
            _ => None,
        },
        AlgorithmParameters::OctetKeyPair(okp) if okp.curve == EllipticCurve::Ed25519 => {
            Some(Algorithm::EdDSA)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{EncodingKey, Header};
    use p256::pkcs8::{EncodePrivateKey, LineEnding};
    use serde_json::json;

    const ISS: &str = "https://idp.example/realms/rostwerk";
    const AUD: &str = "rostwerk-api";

    struct TestKey {
        kid: String,
        encoding: EncodingKey,
        jwk: Value,
    }

    fn test_key(kid: &str) -> TestKey {
        let secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let pem = secret.to_pkcs8_pem(LineEnding::LF).unwrap();
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes()).unwrap();
        let mut jwk =
            serde_json::to_value(Jwk::from_encoding_key(&encoding, Algorithm::ES256).unwrap())
                .unwrap();
        jwk["kid"] = json!(kid);
        jwk["use"] = json!("sig");
        TestKey {
            kid: kid.into(),
            encoding,
            jwk,
        }
    }

    fn now() -> i64 {
        jsonwebtoken::get_current_timestamp() as i64
    }

    fn valid_claims() -> Value {
        json!({ "iss": ISS, "aud": AUD, "sub": "user-1", "exp": now() + 300, "iat": now() })
    }

    fn sign(key: &TestKey, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(key.kid.clone());
        jsonwebtoken::encode(&header, claims, &key.encoding).unwrap()
    }

    fn verifier(keys: &[&TestKey]) -> OidcVerifier {
        let jwks = json!({ "keys": keys.iter().map(|k| k.jwk.clone()).collect::<Vec<_>>() });
        OidcVerifier::with_jwks(ISS, AUD, &jwks)
    }

    async fn verify(v: &OidcVerifier, token: &str) -> Result<Claims, VerifyError> {
        v.verify(token).await
    }

    #[tokio::test]
    async fn accepts_valid_token() {
        let key = test_key("k1");
        let claims = verify(&verifier(&[&key]), &sign(&key, &valid_claims()))
            .await
            .unwrap();
        assert_eq!(claims.sub, "user-1");
    }

    #[tokio::test]
    async fn accepts_audience_list_and_picks_key_by_kid() {
        let (k1, k2) = (test_key("k1"), test_key("k2"));
        let mut claims = valid_claims();
        claims["aud"] = json!(["account", AUD]);
        assert!(
            verify(&verifier(&[&k1, &k2]), &sign(&k2, &claims))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_wrong_issuer_audience_and_times() {
        let key = test_key("k1");
        let v = verifier(&[&key]);
        let cases = [
            ("iss", json!("https://evil.example")),
            ("aud", json!("someone-else")),
            ("exp", json!(now() - 120)),
            ("nbf", json!(now() + 120)),
            ("sub", json!("")),
        ];
        for (claim, value) in cases {
            let mut claims = valid_claims();
            claims[claim] = value;
            assert!(
                verify(&v, &sign(&key, &claims)).await.is_err(),
                "{claim} not checked"
            );
        }
    }

    #[tokio::test]
    async fn rejects_missing_required_claims() {
        let key = test_key("k1");
        let v = verifier(&[&key]);
        for claim in ["iss", "aud", "exp", "sub"] {
            let mut claims = valid_claims();
            claims.as_object_mut().unwrap().remove(claim);
            assert!(
                verify(&v, &sign(&key, &claims)).await.is_err(),
                "{claim} not required"
            );
        }
    }

    #[tokio::test]
    async fn tolerates_small_clock_skew() {
        let key = test_key("k1");
        let mut claims = valid_claims();
        claims["exp"] = json!(now() - 5);
        claims["nbf"] = json!(now() + 5);
        assert!(
            verify(&verifier(&[&key]), &sign(&key, &claims))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_foreign_signature_and_unknown_kid() {
        let (trusted, other) = (test_key("k1"), test_key("k1"));
        let v = verifier(&[&trusted]);
        // Same kid, different key: signature check fails.
        assert!(matches!(
            verify(&v, &sign(&other, &valid_claims())).await,
            Err(VerifyError::Invalid(_))
        ));
        let stranger = test_key("k9");
        assert!(matches!(
            verify(&v, &sign(&stranger, &valid_claims())).await,
            Err(VerifyError::UnknownKey(_))
        ));
    }

    #[tokio::test]
    async fn rejects_hmac_and_unsigned_tokens() {
        let key = test_key("k1");
        let v = verifier(&[&key]);

        // HS256 "signed" with the public key material: classic algorithm confusion.
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some("k1".into());
        let hs = jsonwebtoken::encode(
            &header,
            &valid_claims(),
            &EncodingKey::from_secret(key.jwk.to_string().as_bytes()).unwrap(),
        )
        .unwrap();
        assert!(verify(&v, &hs).await.is_err());

        // alg=none with an empty signature.
        let b64 = |v: &Value| {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
        };
        let none = format!(
            "{}.{}.",
            b64(&json!({ "alg": "none", "kid": "k1" })),
            b64(&valid_claims())
        );
        assert!(verify(&v, &none).await.is_err());
        assert!(verify(&v, "not-a-jwt").await.is_err());
    }

    #[tokio::test]
    async fn skips_unusable_keys_in_jwks() {
        let key = test_key("k1");
        let jwks = json!({ "keys": [
            { "kty": "RSA", "use": "enc", "alg": "RSA-OAEP", "kid": "enc", "n": "AQAB", "e": "AQAB" },
            { "kty": "oct", "kid": "hmac", "k": "c2VjcmV0" },
            { "kty": "weird" },
            key.jwk,
        ]});
        let v = OidcVerifier::with_jwks(ISS, AUD, &jwks);
        assert_eq!(v.cache.read().await.keys.len(), 1);
        assert!(verify(&v, &sign(&key, &valid_claims())).await.is_ok());
    }
}
