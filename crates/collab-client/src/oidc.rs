//! Signing in with OIDC from the desktop (design §12, milestone C5.1b): Authorization Code with
//! PKCE (RFC 7636) for a native app with a loopback redirect (RFC 8252).
//!
//! 1. Discovery (`<issuer>/.well-known/openid-configuration`), whose `issuer` must match.
//! 2. A random `code_verifier`, its S256 `code_challenge`, and a random `state`.
//! 3. A one-shot listener on `127.0.0.1:<free port>`; the system browser opens the provider's
//!    authorization page with `redirect_uri=http://127.0.0.1:<port>/callback`. (Authentik: allow
//!    that redirect URI with any port, e.g. the regex `^http://127\.0\.0\.1:\d+/callback$`.)
//! 4. The browser comes back with `code` and `state`; a wrong `state` or an `error` ends the
//!    sign-in. Other requests on the port (a favicon) get a 404 and are ignored.
//! 5. The code is exchanged at the token endpoint with the verifier.
//!
//! Nothing here logs codes or tokens; [`Tokens`] redacts itself in `Debug`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Longest provider answer read, in bytes.
const MAX_PROVIDER_BYTES: u64 = 1024 * 1024;
/// Longest request read on the loopback port, in bytes.
const MAX_CALLBACK_BYTES: usize = 16 * 1024;

/// A client registered with the provider (a public client: no secret).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcClient {
    pub issuer: String,
    pub client_id: String,
    /// Space-separated; `openid` at least. `offline_access` asks for a refresh token.
    pub scopes: String,
}

/// What the token endpoint gave.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds the access token lasts, as the provider said.
    #[serde(default)]
    pub expires_in: Option<u64>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tokens").field("expires_in", &self.expires_in).field("refresh_token", &self.refresh_token.is_some()).finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SignInError {
    #[error("identity provider: {0}")]
    Provider(String),
    #[error("sign-in was not completed: {0}")]
    Refused(String),
    #[error("sign-in timed out")]
    TimedOut,
    #[error("sign-in: {0}")]
    Local(String),
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(20))).http_status_as_error(false).build().into()
}

fn random_token() -> Result<String, SignInError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| SignInError::Local(e.to_string()))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// `code_challenge` for a verifier (S256).
pub fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn discover(agent: &ureq::Agent, issuer: &str) -> Result<Discovery, SignInError> {
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let text = get_text(agent, &url)?;
    let d: Discovery = serde_json::from_str(&text).map_err(|e| SignInError::Provider(format!("discovery: {e}")))?;
    if d.issuer != issuer {
        return Err(SignInError::Provider(format!("discovery names issuer {:?}, configured {issuer:?}", d.issuer)));
    }
    Ok(d)
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String, SignInError> {
    let mut r = agent.get(url).call().map_err(|e| SignInError::Provider(format!("{url}: {e}")))?;
    if !r.status().is_success() {
        return Err(SignInError::Provider(format!("{url}: HTTP {}", r.status())));
    }
    r.body_mut().with_config().limit(MAX_PROVIDER_BYTES).read_to_string().map_err(|e| SignInError::Provider(format!("{url}: {e}")))
}

/// `application/x-www-form-urlencoded` / query encoding of one value.
fn encode(value: &str) -> String {
    value.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") }).collect()
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        match b {
            b'+' => out.push(b' '),
            b'%' => match bytes.get(i + 1..i + 3).and_then(|h| std::str::from_utf8(h).ok()).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            _ => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        (decode(k) == key).then(|| decode(v))
    })
}

/// Sign in: `open` shows the authorization URL to the user (the system browser in the app,
/// [`open_in_browser`]); this waits up to `timeout` for the browser to come back. Blocking.
pub fn sign_in(client: &OidcClient, open: impl FnOnce(&str) -> Result<(), String>, timeout: Duration) -> Result<Tokens, SignInError> {
    let agent = agent();
    let discovery = discover(&agent, &client.issuer)?;
    let (verifier, state) = (random_token()?, random_token()?);
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| SignInError::Local(e.to_string()))?;
    let port = listener.local_addr().map_err(|e| SignInError::Local(e.to_string()))?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    let separator = if discovery.authorization_endpoint.contains('?') { '&' } else { '?' };
    let url = format!(
        "{}{separator}response_type=code&client_id={}&redirect_uri={}&scope={}&state={}&code_challenge={}&code_challenge_method=S256",
        discovery.authorization_endpoint,
        encode(&client.client_id),
        encode(&redirect_uri),
        encode(&client.scopes),
        encode(&state),
        encode(&challenge(&verifier)),
    );
    open(&url).map_err(SignInError::Local)?;
    let code = wait_for_code(&listener, &state, timeout)?;
    exchange(
        &agent,
        &discovery.token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("client_id", &client.client_id),
            ("code_verifier", &verifier),
        ],
    )
}

/// A new access token from a refresh token. Blocking.
pub fn refresh(client: &OidcClient, refresh_token: &str) -> Result<Tokens, SignInError> {
    let agent = agent();
    let discovery = discover(&agent, &client.issuer)?;
    exchange(
        &agent,
        &discovery.token_endpoint,
        &[("grant_type", "refresh_token"), ("refresh_token", refresh_token), ("client_id", &client.client_id)],
    )
}

/// Open `url` in the system browser.
pub fn open_in_browser(url: &str) -> Result<(), String> {
    webbrowser::open(url).map_err(|e| format!("cannot open the browser: {e}"))
}

fn exchange(agent: &ureq::Agent, token_endpoint: &str, form: &[(&str, &str)]) -> Result<Tokens, SignInError> {
    let mut r = agent.post(token_endpoint).send_form(form.iter().copied()).map_err(|e| SignInError::Provider(format!("token endpoint: {e}")))?;
    let status = r.status();
    let text =
        r.body_mut().with_config().limit(MAX_PROVIDER_BYTES).read_to_string().map_err(|e| SignInError::Provider(format!("token endpoint: {e}")))?;
    if !status.is_success() {
        // The provider's error code only (RFC 6749 §5.2), never the whole answer.
        let error = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned));
        return Err(SignInError::Refused(error.unwrap_or_else(|| format!("HTTP {status}"))));
    }
    let tokens: Tokens = serde_json::from_str(&text).map_err(|e| SignInError::Provider(format!("token endpoint: {e}")))?;
    if tokens.access_token.is_empty() {
        return Err(SignInError::Provider("token endpoint: no access token".into()));
    }
    Ok(tokens)
}

/// Serve the loopback port until the browser brings a `code` for `state` (or an error).
fn wait_for_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String, SignInError> {
    listener.set_nonblocking(true).map_err(|e| SignInError::Local(e.to_string()))?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(result) = callback(stream, state) {
                    return result;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(SignInError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(SignInError::Local(e.to_string())),
        }
    }
}

/// One request on the loopback port: the sign-in's outcome, or `None` for anything else.
fn callback(mut stream: TcpStream, state: &str) -> Option<Result<String, SignInError>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = vec![0u8; MAX_CALLBACK_BYTES];
    let mut len = 0;
    while len < buf.len() {
        match stream.read(buf.get_mut(len..)?) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                len += n;
                if buf.get(..len).is_some_and(|b| b.windows(4).any(|w| w == b"\r\n\r\n")) {
                    break;
                }
            }
        }
    }
    let request = String::from_utf8_lossy(buf.get(..len).unwrap_or_default()).into_owned();
    let target = request.lines().next().and_then(|line| line.strip_prefix("GET ")).and_then(|rest| rest.split(' ').next()).unwrap_or("");
    let Some(query) = target.strip_prefix("/callback?") else {
        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return None;
    };
    let outcome = if query_param(query, "state").as_deref() != Some(state) {
        Err(SignInError::Refused("the answer did not belong to this sign-in (state mismatch)".into()))
    } else if let Some(error) = query_param(query, "error") {
        Err(SignInError::Refused(error))
    } else {
        query_param(query, "code").filter(|c| !c.is_empty()).ok_or_else(|| SignInError::Refused("no authorization code".into()))
    };
    let body = if outcome.is_ok() {
        "<!doctype html><meta charset=utf-8><title>WordCraft</title><p>Signed in. You can close this window and return to WordCraft.</p>"
    } else {
        "<!doctype html><meta charset=utf-8><title>WordCraft</title><p>Sign-in was not completed. Return to WordCraft and try again.</p>"
    };
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s256_challenge_matches_rfc_7636() {
        // RFC 7636 Appendix B.
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn query_values_round_trip() {
        let v = "a b&c=d/ñ+%";
        assert_eq!(decode(&encode(v)), v);
        assert_eq!(query_param("code=x%2By&state=s1", "code").as_deref(), Some("x+y"));
        assert_eq!(query_param("code=&state=s1", "nope"), None);
        assert_eq!(decode("%zz%4"), "%zz%4");
    }

    #[test]
    fn tokens_never_show_in_debug() {
        let t = Tokens { access_token: "secret-access".into(), refresh_token: Some("secret-refresh".into()), expires_in: Some(300) };
        let shown = format!("{t:?}");
        assert!(!shown.contains("secret"));
    }
}
