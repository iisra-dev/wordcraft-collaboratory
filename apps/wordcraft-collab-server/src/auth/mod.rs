//! Authentication and authorisation (design §12, milestone C5.1).
//!
//! The desktop client signs in with OIDC Authorization Code + PKCE against Authentik (system
//! browser, loopback callback with `state`), then opens the WSS connection with a short-lived
//! access token, sent in a header, never in the URL or logs. The server validates issuer,
//! audience, signature and expiry, maps the subject to `users.oidc_subject`, and checks the
//! member's role (`owner`, `editor`, `viewer`) on every operation and every HTTP endpoint.
//!
//! Optional local accounts are for setups without an identity provider.
