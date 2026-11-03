//! The server starts on a fresh data directory, answers its health endpoints and the
//! `healthcheck` probe the container uses, and shuts down cleanly.

use wordcraft_collab_server::{AppState, Config, healthcheck, serve};

#[tokio::test(flavor = "multi_thread")]
async fn health_endpoints_answer() {
    let dir = std::env::temp_dir().join(format!("wordcraft-collab-health-{}", std::process::id()));
    let config = Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        database_path: dir.join("wordcraft.db"),
        document_storage: dir.join("documents"),
        oidc: None,
    };
    let state = AppState::open(&config).unwrap();
    let listener = tokio::net::TcpListener::bind(config.listen_addr).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(serve(listener, state, async {
        let _ = stopped.await;
    }));

    let (live, ready, missing, probe) = tokio::task::spawn_blocking(move || {
        (
            healthcheck::status(addr, "/api/v1/health/live").unwrap(),
            healthcheck::status(addr, "/api/v1/health/ready").unwrap(),
            healthcheck::status(addr, "/api/v1/nope").unwrap(),
            healthcheck::probe(addr),
        )
    })
    .await
    .unwrap();
    assert_eq!((live, ready, missing), (200, 200, 404));
    probe.unwrap();

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}
