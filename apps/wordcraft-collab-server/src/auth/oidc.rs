//! OIDC access tokens (design §12, milestone C5.1).
//!
//! The desktop client signs in against the identity provider (Authentik) and sends its access
//! token as `Authorization: Bearer <JWT>`. The server checks, for every connection and request:
//! - the signature, with the provider's published keys (discovery → `jwks_uri`), cached and
//!   refetched for an unknown `kid` at most once a minute, so a forged `kid` cannot make the
//!   server hammer the provider;
//! - only asymmetric algorithms (RS*, PS*, ES*): an HMAC algorithm would let a token signed with
//!   the *public* key pass (algorithm confusion), and `none` is never accepted;
//! - `iss` equal to `OIDC_ISSUER` (exactly, as the provider writes it), `aud` containing
//!   `OIDC_AUDIENCE`, `exp` in the future and `nbf` past (60 s leeway), and a `sub`.
//!
//! Provider answers are size-capped and time-limited; tokens are size-capped before parsing.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::{Jwk, JwkSet, KeyAlgorithm};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

/// Longest token looked at, in bytes.
pub const MAX_TOKEN_BYTES: usize = 16 * 1024;
/// Longest provider answer (discovery, keys), in bytes.
const MAX_PROVIDER_BYTES: u64 = 1024 * 1024;
/// Keys are fetched again for an unknown `kid` at most this often.
const REFETCH_AFTER: Duration = Duration::from_secs(60);
/// Clock skew accepted on `exp` and `nbf`.
const LEEWAY_SECS: u64 = 60;
/// Longest `sub` accepted.
const MAX_SUBJECT_BYTES: usize = 255;

const ALGORITHMS: &[Algorithm] =
    &[Algorithm::RS256, Algorithm::RS384, Algorithm::RS512, Algorithm::PS256, Algorithm::PS384, Algorithm::PS512, Algorithm::ES256, Algorithm::ES384];

#[derive(Debug, thiserror::Error)]
pub enum OidcError {
    #[error("identity provider: {0}")]
    Provider(String),
    #[error("token refused: {0}")]
    Token(String),
}

/// What the server takes from a valid token.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Claims {
    pub sub: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
}

#[derive(Default)]
struct Keys {
    set: Option<JwkSet>,
    fetched: Option<Instant>,
}

/// Validates access tokens from one provider for one audience.
pub struct Verifier {
    issuer: String,
    audience: String,
    keys: Mutex<Keys>,
    agent: ureq::Agent,
}

impl Verifier {
    pub fn new(issuer: &str, audience: &str) -> Self {
        let config = ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(10))).build();
        Self { issuer: issuer.to_string(), audience: audience.to_string(), keys: Mutex::new(Keys::default()), agent: config.into() }
    }

    /// The claims of a valid token. Blocking (may fetch the provider's keys).
    pub fn verify(&self, token: &str) -> Result<Claims, OidcError> {
        if token.len() > MAX_TOKEN_BYTES {
            return Err(OidcError::Token("too long".into()));
        }
        let header = decode_header(token).map_err(|e| OidcError::Token(e.to_string()))?;
        if !ALGORITHMS.contains(&header.alg) {
            return Err(OidcError::Token(format!("algorithm {:?} is not accepted", header.alg)));
        }
        let jwk = self.key(header.kid.as_deref(), header.alg)?;
        let key = DecodingKey::from_jwk(&jwk).map_err(|e| OidcError::Provider(format!("unusable key: {e}")))?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY_SECS;
        let claims = decode::<Claims>(token, &key, &validation).map_err(|e| OidcError::Token(e.to_string()))?.claims;
        if claims.sub.is_empty() || claims.sub.len() > MAX_SUBJECT_BYTES || claims.sub.chars().any(char::is_control) {
            return Err(OidcError::Token("unusable subject".into()));
        }
        Ok(claims)
    }

    /// The signing key for `kid` (the only key when the token names none), fetching the key set
    /// when it is not known yet or, rate-limited, when `kid` is not in it.
    fn key(&self, kid: Option<&str>, alg: Algorithm) -> Result<Jwk, OidcError> {
        let mut keys = self.keys.lock().map_err(|_| OidcError::Provider("key cache lock poisoned".into()))?;
        if let Some(found) = keys.set.as_ref().and_then(|set| pick(set, kid, alg)) {
            return Ok(found);
        }
        let stale = keys.fetched.is_none_or(|at| at.elapsed() >= REFETCH_AFTER);
        if !stale {
            return Err(OidcError::Token("signed with an unknown key".into()));
        }
        keys.fetched = Some(Instant::now());
        keys.set = Some(self.fetch_keys()?);
        keys.set.as_ref().and_then(|set| pick(set, kid, alg)).ok_or_else(|| OidcError::Token("signed with an unknown key".into()))
    }

    fn fetch_keys(&self) -> Result<JwkSet, OidcError> {
        #[derive(Deserialize)]
        struct Discovery {
            issuer: String,
            jwks_uri: String,
        }
        let discovery_url = format!("{}/.well-known/openid-configuration", self.issuer.trim_end_matches('/'));
        let discovery: Discovery = serde_json::from_str(&self.get(&discovery_url)?).map_err(|e| OidcError::Provider(format!("discovery: {e}")))?;
        // OpenID Connect Discovery §4.3: the document must name the issuer it was fetched for.
        if discovery.issuer != self.issuer {
            return Err(OidcError::Provider(format!("discovery names issuer {:?}, configured {:?}", discovery.issuer, self.issuer)));
        }
        serde_json::from_str(&self.get(&discovery.jwks_uri)?).map_err(|e| OidcError::Provider(format!("keys: {e}")))
    }

    fn get(&self, url: &str) -> Result<String, OidcError> {
        let mut response = self.agent.get(url).call().map_err(|e| OidcError::Provider(format!("{url}: {e}")))?;
        response.body_mut().with_config().limit(MAX_PROVIDER_BYTES).read_to_string().map_err(|e| OidcError::Provider(format!("{url}: {e}")))
    }
}

/// The key a token names: by `kid`, or the only signing key when there is no `kid`. A key that
/// declares another algorithm or another use than signing is never used.
fn pick(set: &JwkSet, kid: Option<&str>, alg: Algorithm) -> Option<Jwk> {
    let usable = |k: &&Jwk| {
        let alg_ok = k.common.key_algorithm.is_none_or(|a| key_algorithm(alg) == Some(a));
        let use_ok = k.common.public_key_use.as_ref().is_none_or(|u| matches!(u, jsonwebtoken::jwk::PublicKeyUse::Signature));
        alg_ok && use_ok
    };
    match kid {
        Some(kid) => set.keys.iter().filter(usable).find(|k| k.common.key_id.as_deref() == Some(kid)).cloned(),
        None => {
            let mut candidates = set.keys.iter().filter(usable);
            let only = candidates.next()?;
            candidates.next().is_none().then(|| only.clone())
        }
    }
}

fn key_algorithm(alg: Algorithm) -> Option<KeyAlgorithm> {
    Some(match alg {
        Algorithm::RS256 => KeyAlgorithm::RS256,
        Algorithm::RS384 => KeyAlgorithm::RS384,
        Algorithm::RS512 => KeyAlgorithm::RS512,
        Algorithm::PS256 => KeyAlgorithm::PS256,
        Algorithm::PS384 => KeyAlgorithm::PS384,
        Algorithm::PS512 => KeyAlgorithm::PS512,
        Algorithm::ES256 => KeyAlgorithm::ES256,
        Algorithm::ES384 => KeyAlgorithm::ES384,
        _ => return None,
    })
}
