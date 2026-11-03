//! C5.1b: the desktop sign-in (Authorization Code + PKCE, loopback redirect) against a simulated
//! provider that checks what a real one checks: the S256 challenge against the verifier, the
//! redirect URI and client id, and a one-time code. The "browser" is an HTTP client that follows
//! the provider's redirect back to the loopback port, as a real browser would.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Form, Query};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::json;
use wordcraft_collab_client::oidc::{OidcClient, SignInError, challenge, refresh, sign_in};

/// How the simulated provider answers the authorization request.
#[derive(Clone, Copy)]
enum Behaviour {
    Grant,
    Deny,
    WrongState,
}

#[derive(Default)]
struct Issued {
    /// code → (challenge, redirect_uri)
    codes: HashMap<String, (String, String)>,
}

async fn provider(behaviour: Behaviour) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let issuer = format!("{base}/application/o/wordcraft/");
    let issued = Arc::new(Mutex::new(Issued::default()));
    let discovery = json!({
        "issuer": issuer,
        "authorization_endpoint": format!("{base}/application/o/authorize/"),
        "token_endpoint": format!("{base}/application/o/token/"),
    });
    let (for_authorize, for_token) = (issued.clone(), issued.clone());
    let app = axum::Router::new()
        .route("/application/o/wordcraft/.well-known/openid-configuration", get(move || async move { Json(discovery) }))
        .route(
            "/application/o/authorize/",
            get(move |Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!(q.get("response_type").map(String::as_str), Some("code"));
                assert_eq!(q.get("code_challenge_method").map(String::as_str), Some("S256"));
                assert_eq!(q.get("client_id").map(String::as_str), Some("wordcraft-desktop"));
                let redirect = q["redirect_uri"].clone();
                assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"), "{redirect}");
                let code = "one-time-code".to_string();
                for_authorize.lock().unwrap().codes.insert(code.clone(), (q["code_challenge"].clone(), redirect.clone()));
                let location = match behaviour {
                    Behaviour::Grant => format!("{redirect}?code={code}&state={}", q["state"]),
                    Behaviour::Deny => format!("{redirect}?error=access_denied&state={}", q["state"]),
                    Behaviour::WrongState => format!("{redirect}?code={code}&state=forged"),
                };
                Response::builder().status(StatusCode::FOUND).header(header::LOCATION, location).body(axum::body::Body::empty()).unwrap()
            }),
        )
        .route(
            "/application/o/token/",
            post(move |Form(f): Form<HashMap<String, String>>| async move {
                let bad = |e: &str| (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response();
                match f.get("grant_type").map(String::as_str) {
                    Some("authorization_code") => {
                        // One-time code: taken out on first use.
                        let Some((expected_challenge, redirect)) = for_token.lock().unwrap().codes.remove(&f["code"]) else {
                            return bad("invalid_grant");
                        };
                        if challenge(&f["code_verifier"]) != expected_challenge
                            || f["redirect_uri"] != redirect
                            || f["client_id"] != "wordcraft-desktop"
                        {
                            return bad("invalid_grant");
                        }
                        Json(json!({"access_token": "access-1", "refresh_token": "refresh-1", "expires_in": 300, "token_type": "Bearer"}))
                            .into_response()
                    }
                    Some("refresh_token") if f.get("refresh_token").map(String::as_str) == Some("refresh-1") => {
                        Json(json!({"access_token": "access-2", "expires_in": 300, "token_type": "Bearer"})).into_response()
                    }
                    _ => bad("invalid_grant"),
                }
            }),
        );
    tokio::spawn(async move { axum::serve(listener, app).await });
    issuer
}

fn client(issuer: &str) -> OidcClient {
    OidcClient { issuer: issuer.into(), client_id: "wordcraft-desktop".into(), scopes: "openid profile offline_access".into() }
}

/// A browser: follows the provider's redirect back to the app's loopback port.
fn browser(url: &str) -> Result<(), String> {
    let url = url.to_string();
    std::thread::spawn(move || {
        let _ = ureq::get(&url).call();
    });
    Ok(())
}

async fn run(issuer: String, timeout: Duration) -> Result<wordcraft_collab_client::oidc::Tokens, SignInError> {
    tokio::task::spawn_blocking(move || sign_in(&client(&issuer), browser, timeout)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn signing_in_gets_tokens_and_refreshes_them() {
    let issuer = provider(Behaviour::Grant).await;
    let tokens = run(issuer.clone(), Duration::from_secs(10)).await.unwrap();
    assert_eq!((tokens.access_token.as_str(), tokens.refresh_token.as_deref(), tokens.expires_in), ("access-1", Some("refresh-1"), Some(300)));
    let refreshed = tokio::task::spawn_blocking(move || refresh(&client(&issuer), "refresh-1")).await.unwrap().unwrap();
    assert_eq!(refreshed.access_token, "access-2");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_or_forged_answer_ends_the_sign_in() {
    let denied = run(provider(Behaviour::Deny).await, Duration::from_secs(10)).await.unwrap_err();
    assert_eq!(denied, SignInError::Refused("access_denied".into()));
    let forged = run(provider(Behaviour::WrongState).await, Duration::from_secs(10)).await.unwrap_err();
    assert!(matches!(forged, SignInError::Refused(ref m) if m.contains("state")), "{forged:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_refresh_token_and_a_browser_that_never_returns_fail_cleanly() {
    let issuer = provider(Behaviour::Grant).await;
    let i = issuer.clone();
    let err = tokio::task::spawn_blocking(move || refresh(&client(&i), "stolen")).await.unwrap().unwrap_err();
    assert_eq!(err, SignInError::Refused("invalid_grant".into()));
    let never = tokio::task::spawn_blocking(move || sign_in(&client(&issuer), |_| Ok(()), Duration::from_millis(300))).await.unwrap();
    assert_eq!(never.unwrap_err(), SignInError::TimedOut);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_source_renews_before_expiry_and_when_refused() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wordcraft_collab_client::oidc::{TokenSource, Tokens};
    let issuer = provider(Behaviour::Grant).await;
    tokio::task::spawn_blocking(move || {
        let fresh = Tokens { access_token: "access-1".into(), refresh_token: Some("refresh-1".into()), expires_in: Some(300) };
        let renewals = Arc::new(AtomicUsize::new(0));
        let counted = renewals.clone();
        let source = TokenSource::new(client(&issuer), &fresh).on_renew(move |t| {
            // The provider keeps refresh tokens: the old one is handed on to be kept.
            assert_eq!(t.refresh_token.as_deref(), Some("refresh-1"));
            counted.fetch_add(1, Ordering::SeqCst);
        });
        // Still valid for minutes: no network.
        assert_eq!(source.token(false).unwrap(), "access-1");
        assert_eq!(renewals.load(Ordering::SeqCst), 0);
        // The server refused it: renewed.
        assert_eq!(source.token(true).unwrap(), "access-2");
        assert_eq!(renewals.load(Ordering::SeqCst), 1);
        // About to expire: renewed without being asked.
        let expiring = Tokens { access_token: "access-1".into(), refresh_token: Some("refresh-1".into()), expires_in: Some(30) };
        assert_eq!(TokenSource::new(client(&issuer), &expiring).token(false).unwrap(), "access-2");
        // From the OS store after a restart: the first token renews.
        assert_eq!(TokenSource::restore(client(&issuer), "refresh-1".into()).token(false).unwrap(), "access-2");
        assert_eq!(TokenSource::restore(client(&issuer), "stolen".into()).token(false).unwrap_err(), SignInError::Refused("invalid_grant".into()));
        // No refresh token: the current token is all there is.
        let bare = Tokens { access_token: "access-1".into(), refresh_token: None, expires_in: Some(30) };
        assert_eq!(TokenSource::new(client(&issuer), &bare).token(true).unwrap(), "access-1");
    })
    .await
    .unwrap();
}
