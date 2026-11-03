//! Configuration from the environment (see `deploy/.env.example` and `deploy/compose.yaml`).

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `LISTEN_ADDR`, default `127.0.0.1:3000` (the container sets `0.0.0.0:3000`).
    pub listen_addr: SocketAddr,
    /// `DATABASE_URL`: `sqlite:///data/wordcraft.db` or a plain path; default `data/wordcraft.db`.
    pub database_path: PathBuf,
    /// `DOCUMENT_STORAGE`: snapshot directory; default `data/documents`.
    pub document_storage: PathBuf,
    /// `OIDC_ISSUER` + `OIDC_AUDIENCE` (both or neither). Unset = no sign-in yet (milestone C5.1);
    /// never expose such a server beyond localhost.
    pub oidc: Option<OidcConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OidcConfig {
    pub issuer: String,
    pub audience: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{var}: `{value}` is not a socket address (e.g. 0.0.0.0:3000)")]
    BadAddr { var: &'static str, value: String },
    #[error("DATABASE_URL: only sqlite URLs are supported, got `{0}`")]
    BadDatabaseUrl(String),
    #[error("OIDC_ISSUER and OIDC_AUDIENCE must be set together")]
    PartialOidc,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    /// Build from any variable lookup (tests pass a map).
    pub fn from_lookup(var: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let addr = var("LISTEN_ADDR").unwrap_or_else(|| "127.0.0.1:3000".into());
        let listen_addr = addr.parse().map_err(|_| ConfigError::BadAddr { var: "LISTEN_ADDR", value: addr.clone() })?;
        let database_path = match var("DATABASE_URL") {
            None => PathBuf::from("data/wordcraft.db"),
            Some(url) => sqlite_path(&url).ok_or(ConfigError::BadDatabaseUrl(url))?,
        };
        let document_storage = var("DOCUMENT_STORAGE").map_or_else(|| PathBuf::from("data/documents"), PathBuf::from);
        let oidc = match (var("OIDC_ISSUER"), var("OIDC_AUDIENCE")) {
            (Some(issuer), Some(audience)) => Some(OidcConfig { issuer, audience }),
            (None, None) => None,
            _ => return Err(ConfigError::PartialOidc),
        };
        Ok(Self { listen_addr, database_path, document_storage, oidc })
    }
}

/// `sqlite:///abs/path`, `sqlite://rel/path`, `sqlite:path` or a bare path.
fn sqlite_path(url: &str) -> Option<PathBuf> {
    let path = match url.strip_prefix("sqlite:") {
        Some(rest) => rest.strip_prefix("//").unwrap_or(rest),
        None if url.contains("://") => return None,
        None => url,
    };
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = vars.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        Config::from_lookup(|k| map.get(k).cloned())
    }

    #[test]
    fn compose_settings_parse() {
        let c = config(&[
            ("LISTEN_ADDR", "0.0.0.0:3000"),
            ("DATABASE_URL", "sqlite:///data/wordcraft.db"),
            ("DOCUMENT_STORAGE", "/data/documents"),
            ("OIDC_ISSUER", "https://auth.example.com/application/o/wordcraft/"),
            ("OIDC_AUDIENCE", "wordcraft"),
        ])
        .unwrap();
        assert_eq!(c.database_path, PathBuf::from("/data/wordcraft.db"));
        assert_eq!(c.listen_addr.port(), 3000);
        assert!(c.oidc.is_some());
    }

    #[test]
    fn bad_settings_are_errors() {
        assert!(config(&[]).unwrap().oidc.is_none());
        assert!(matches!(config(&[("LISTEN_ADDR", "nope")]), Err(ConfigError::BadAddr { .. })));
        assert!(matches!(config(&[("DATABASE_URL", "postgres://x")]), Err(ConfigError::BadDatabaseUrl(_))));
        assert!(matches!(config(&[("DATABASE_URL", "sqlite://")]), Err(ConfigError::BadDatabaseUrl(_))));
        assert!(matches!(config(&[("OIDC_ISSUER", "https://a")]), Err(ConfigError::PartialOidc)));
    }
}
