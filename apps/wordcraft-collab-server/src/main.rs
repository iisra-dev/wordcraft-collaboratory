//! `wordcraft-collab-server [serve]` runs the server; `wordcraft-collab-server healthcheck` probes
//! a running one (the container healthcheck); `wordcraft-collab-server create-user <name>` adds a
//! local user and prints their sign-in token, once. Configuration comes from the environment
//! (`deploy/.env.example`).
//!
//! Backups (C6.2, `wordcraft_collab_server::backup`): `backup [--every 24h]` writes an encrypted
//! backup to `BACKUP_DIR` for `BACKUP_RECIPIENTS` (once, or repeatedly), safe while the server
//! runs; `verify-backup <file|latest> --identity <key file>` proves one restores;
//! `restore <file> --identity <key file> --to <empty dir>` restores one (server stopped).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::process::ExitCode;

use tracing_subscriber::EnvFilter;
use wordcraft_collab_server::{AppState, Config, ServerError, healthcheck};

fn main() -> ExitCode {
    let arg = std::env::args().nth(1);
    match arg.as_deref() {
        None | Some("serve") => report(run_server()),
        Some("healthcheck") => match Config::from_env().map_err(ServerError::from).and_then(|c| Ok(healthcheck::probe(c.listen_addr)?)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("unhealthy: {e}");
                ExitCode::FAILURE
            }
        },
        Some("create-user") => {
            let name = std::env::args().skip(2).collect::<Vec<_>>().join(" ");
            report(create_user(&name))
        }
        Some("backup") => report(backup(&std::env::args().skip(2).collect::<Vec<_>>())),
        Some("verify-backup") => report(verify_backup(&std::env::args().skip(2).collect::<Vec<_>>())),
        Some("restore") => report(restore(&std::env::args().skip(2).collect::<Vec<_>>())),
        Some("-h" | "--help") => {
            println!(
                "usage: wordcraft-collab-server [serve | healthcheck | create-user <display name>\n  \
                 | backup [--every <N>h|<N>m] | verify-backup <file|latest> --identity <key file>\n  \
                 | restore <file> --identity <key file> --to <empty dir>]"
            );
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown subcommand `{other}` (try --help)");
            ExitCode::from(2)
        }
    }
}

fn report(result: Result<(), ServerError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("wordcraft-collab-server: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Add a local user and print their id and token (shown once; only its hash is stored).
fn create_user(name: &str) -> Result<(), ServerError> {
    let config = Config::from_env()?;
    let state = AppState::open(&config)?;
    let (id, token) = wordcraft_collab_server::auth::create_local_user(&state.db, name)?;
    println!("user  {id}\ntoken {token}\n\nThe token is not stored and cannot be shown again.");
    Ok(())
}

/// `--name value` in `args`.
fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// `24h`, `90m`: at least a minute, at most a week.
fn interval(text: &str) -> Result<std::time::Duration, ServerError> {
    let bad = || ServerError::Usage(format!("--every takes hours or minutes such as 24h or 90m, not {text:?}"));
    let (number, unit) = text.split_at_checked(text.len().saturating_sub(1)).ok_or_else(bad)?;
    let n: u64 = number.parse().map_err(|_| bad())?;
    let minutes = match unit {
        "h" => n.saturating_mul(60),
        "m" => n,
        _ => return Err(bad()),
    };
    if !(1..=7 * 24 * 60).contains(&minutes) {
        return Err(bad());
    }
    Ok(std::time::Duration::from_secs(minutes * 60))
}

/// One backup now, or one every `--every`; a failed round is reported and tried again next time.
fn backup(args: &[String]) -> Result<(), ServerError> {
    use wordcraft_collab_server::backup::{self, BackupConfig};
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();
    let config = Config::from_env()?;
    let settings = BackupConfig::from_env()?;
    let every = flag(args, "--every").map(interval).transpose()?;
    loop {
        match backup::create(&config, &settings) {
            Ok(s) => tracing::info!(file = %s.path.display(), documents = s.documents, snapshots = s.snapshots, bytes = s.bytes, "backup written"),
            Err(e) if every.is_some() => tracing::error!("backup failed (next attempt in one interval): {e}"),
            Err(e) => return Err(e.into()),
        }
        match every {
            Some(every) => std::thread::sleep(every),
            None => return Ok(()),
        }
    }
}

fn identities(args: &[String]) -> Result<Vec<Box<dyn age::Identity>>, ServerError> {
    let file = flag(args, "--identity").ok_or_else(|| ServerError::Usage("give the private key file with --identity <file>".into()))?;
    Ok(wordcraft_collab_server::backup::identities(std::path::Path::new(file))?)
}

fn backup_file(args: &[String]) -> Result<std::path::PathBuf, ServerError> {
    let name = args.first().filter(|a| !a.starts_with("--")).ok_or_else(|| ServerError::Usage("name the backup file (or `latest`)".into()))?;
    if name == "latest" {
        let dir = wordcraft_collab_server::backup::dir_from_env();
        let newest = wordcraft_collab_server::backup::list(&dir)?.pop();
        return newest.ok_or_else(|| ServerError::Usage(format!("no backups in {}", dir.display())));
    }
    Ok(std::path::PathBuf::from(name))
}

/// Prove a backup restores: decrypt, check, open every document, in a scratch directory.
fn verify_backup(args: &[String]) -> Result<(), ServerError> {
    let file = backup_file(args)?;
    let summary = wordcraft_collab_server::backup::verify(&file, &identities(args)?)?;
    println!("{} restores: {} documents, {} snapshots, {} bytes.", file.display(), summary.documents, summary.snapshots, summary.bytes);
    Ok(())
}

/// Restore a backup into an empty directory (the server stopped), then point it there.
fn restore(args: &[String]) -> Result<(), ServerError> {
    let file = backup_file(args)?;
    let target = flag(args, "--to").ok_or_else(|| ServerError::Usage("give an empty directory with --to <dir>".into()))?;
    let summary = wordcraft_collab_server::backup::restore(&file, &identities(args)?, std::path::Path::new(target))?;
    println!(
        "Restored {} documents ({} snapshots) into {}.\nPoint DATABASE_URL at {target}/wordcraft.db and DOCUMENT_STORAGE at {target}/documents, then start the server.",
        summary.documents,
        summary.snapshots,
        summary.path.display()
    );
    Ok(())
}

fn run_server() -> Result<(), ServerError> {
    tracing_subscriber::fmt().with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"))).init();
    let config = Config::from_env()?;
    let state = AppState::open(&config)?;
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
        tracing::info!(addr = %config.listen_addr, db = %config.database_path.display(), "listening");
        wordcraft_collab_server::serve(listener, state, async {
            if tokio::signal::ctrl_c().await.is_err() {
                std::future::pending::<()>().await;
            }
        })
        .await
    })
}

#[cfg(test)]
mod tests {
    use super::interval;
    use std::time::Duration;

    #[test]
    fn backup_intervals_reject_hostile_and_unicode_values_without_panicking() {
        for value in ["", "ñ", "1📝", "24小时", "0m", "169h", "18446744073709551615h", "1s", "-1h"] {
            assert!(interval(value).is_err(), "{value:?}");
        }
        for (value, seconds) in [("1m", 60), ("90m", 5400), ("24h", 86400), ("168h", 604800)] {
            assert_eq!(interval(value).unwrap(), Duration::from_secs(seconds));
        }
    }
}
