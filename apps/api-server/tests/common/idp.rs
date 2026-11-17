//! A fake OIDC provider for tests: serves discovery and a JWKS on a local
//! port and signs access tokens with keys generated in the test. No network.

use std::sync::{Arc, RwLock};

use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::jwk::Jwk;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use p256::pkcs8::{EncodePrivateKey, LineEnding};
use serde_json::{Value, json};

pub const AUDIENCE: &str = "rostwerk-api";

pub struct SigningKey {
    pub kid: String,
    encoding: EncodingKey,
    jwk: Value,
}

impl SigningKey {
    pub fn generate(kid: &str) -> Self {
        let secret = p256::SecretKey::random(&mut rand_core::OsRng);
        let pem = secret.to_pkcs8_pem(LineEnding::LF).unwrap();
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes()).unwrap();
        let mut jwk =
            serde_json::to_value(Jwk::from_encoding_key(&encoding, Algorithm::ES256).unwrap())
                .unwrap();
        jwk["kid"] = json!(kid);
        jwk["use"] = json!("sig");
        Self {
            kid: kid.into(),
            encoding,
            jwk,
        }
    }

    pub fn sign(&self, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(self.kid.clone());
        jsonwebtoken::encode(&header, claims, &self.encoding).unwrap()
    }
}

pub struct Idp {
    pub issuer: String,
    pub key: SigningKey,
    /// The published key set; tests may replace it to simulate rotation.
    pub jwks: Arc<RwLock<Value>>,
}

impl Idp {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = format!("http://{}", listener.local_addr().unwrap());
        let key = SigningKey::generate("test-key-1");
        let jwks = Arc::new(RwLock::new(json!({ "keys": [key.jwk.clone()] })));

        let discovery = json!({
            "issuer": issuer,
            "jwks_uri": format!("{issuer}/protocol/openid-connect/certs"),
        });
        let published = jwks.clone();
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move { Json(discovery) }),
            )
            .route(
                "/protocol/openid-connect/certs",
                get(move || async move { Json(published.read().unwrap().clone()) }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { issuer, key, jwks }
    }

    /// Publishes `key` in addition to the current keys.
    pub fn publish(&self, key: &SigningKey) {
        self.jwks.write().unwrap()["keys"]
            .as_array_mut()
            .unwrap()
            .push(key.jwk.clone());
    }

    /// Claims of a valid access token for `sub`.
    pub fn claims(&self, sub: &str) -> Value {
        let now = jsonwebtoken::get_current_timestamp();
        json!({
            "iss": self.issuer,
            "aud": ["account", AUDIENCE],
            "sub": sub,
            "iat": now,
            "exp": now + 300,
        })
    }

    pub fn token(&self, sub: &str) -> String {
        self.key.sign(&self.claims(sub))
    }
}
