//! `wordcraft-collab-server [serve]` runs the server; `wordcraft-collab-server healthcheck` probes
//! a running one (the container healthcheck). Configuration comes from the environment
//! (`deploy/.env.example`).
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
        Some("-h" | "--help") => {
            println!("usage: wordcraft-collab-server [serve | healthcheck]");
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
