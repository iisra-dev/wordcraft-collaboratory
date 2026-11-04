//! C6.5a: invitations through the real router, actor permissions, transaction rollback and restore.

use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId, RejectReason, ServerMessage};
use wordcraft_collab_server::documents::Submission;
use wordcraft_collab_server::persistence::{Role, UserId, links, repository};
use wordcraft_collab_server::{AppState, Config, api, auth, backup};

struct Fixture {
    state: AppState,
    config: Config,
    _dir: tempfile::TempDir,
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap()
    }
    fn private(&self) {
        assert_eq!(self.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(self.headers[header::REFERRER_POLICY], "no-referrer");
    }
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = config_in(dir.path());
        Self { state: AppState::open(&config).unwrap(), config, _dir: dir }
    }

    fn user(&self, name: &str) -> (UserId, String) {
        auth::create_local_user(&self.state.db, name).unwrap()
    }

    async fn call(&self, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Reply {
        call(&self.state, method, uri, token, body).await
    }

    async fn document(&self, token: &str) -> DocumentId {
        let created = self.call("POST", "/api/v1/documents", Some(token), Some(json!({"title":"Private plan ñ"}))).await;
        assert_eq!(created.status, StatusCode::CREATED);
        DocumentId(Uuid::parse_str(created.json()["id"].as_str().unwrap()).unwrap())
    }

    async fn link(&self, document: DocumentId, token: &str, options: Value) -> Value {
        let reply = self.call("POST", &root(document), Some(token), Some(options)).await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.text);
        reply.private();
        reply.json()
    }

    fn count(&self, table: &str) -> i64 {
        self.state.db.lock().unwrap().query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }
}

fn config_in(dir: &std::path::Path) -> Config {
    Config {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        database_path: dir.join("wordcraft.db"),
        document_storage: dir.join("documents"),
        oidc: None,
    }
}

async fn call(state: &AppState, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> Reply {
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let bytes = if let Some(body) = body {
        request = request.header(header::CONTENT_TYPE, "application/json");
        serde_json::to_vec(&body).unwrap()
    } else {
        Vec::new()
    };
    let response = api::router(state.clone()).oneshot(request.body(Body::from(bytes)).unwrap()).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    Reply { status, headers, text: String::from_utf8(bytes.to_vec()).unwrap() }
}

fn root(document: DocumentId) -> String {
    format!("/api/v1/documents/{}/links", document.0)
}
fn join(link: &Value) -> String {
    format!("/api/v1/links/{}/join", link["token"].as_str().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_manages_links_and_only_hashes_are_stored() {
    let fx = Fixture::new();
    let (owner_id, owner) = fx.user("Owner");
    let (viewer_id, viewer) = fx.user("Viewer");
    let (_, stranger) = fx.user("Stranger");
    let document = fx.document(&owner).await;
    let handle = fx.state.documents.get(document).await.unwrap();
    handle.change_member(owner_id, viewer_id, Some(Role::Viewer)).await.unwrap();
    let link = fx.link(document, &owner, json!({"role":"editor"})).await;
    let token = link["token"].as_str().unwrap();
    assert_eq!(token.len(), 68);
    assert_eq!(link["path"], format!("/s/{token}"));
    let hash: String = fx.state.db.lock().unwrap().query_row("SELECT token_sha256 FROM share_links", [], |r| r.get(0)).unwrap();
    assert_ne!(hash, token);
    assert_eq!(links::token_hash(token).unwrap(), hash);
    let list = fx.call("GET", &root(document), Some(&owner), None).await;
    list.private();
    assert!(!list.text.contains(token) && !list.text.contains("sha256"));
    assert_eq!(list.json()[0]["id"], link["id"]);
    let deletion = format!("{}/{}", root(document), link["id"].as_str().unwrap());
    for (method, path, body) in
        [("GET", root(document), None), ("POST", root(document), Some(json!({"role":"viewer"}))), ("DELETE", deletion.clone(), None)]
    {
        assert_eq!(fx.call(method, &path, Some(&viewer), body.clone()).await.status, StatusCode::FORBIDDEN);
        let denied = fx.call(method, &path, Some(&stranger), body.clone()).await;
        let absent = fx.call(method, &path.replace(&document.0.to_string(), &Uuid::nil().to_string()), Some(&stranger), body.clone()).await;
        assert_eq!((denied.status, denied.text), (absent.status, absent.text));
        assert_eq!(fx.call(method, &path, None, body).await.status, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(fx.call("DELETE", &deletion, Some(&owner), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(fx.call("DELETE", &deletion, Some(&owner), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(fx.call("POST", &join(&link), Some(&viewer), Some(json!({}))).await.status, StatusCode::NOT_FOUND);
    let trail = fx.call("GET", &format!("/api/v1/documents/{}/audit", document.0), Some(&owner), None).await;
    assert!(!trail.text.contains(token) && !trail.text.contains("Private plan"));
    assert!(trail.text.contains("share_link_created") && trail.text.contains("share_link_revoked"));
}

#[tokio::test(flavor = "multi_thread")]
async fn joining_promotes_live_permissions_but_never_demotes_existing_members() {
    let fx = Fixture::new();
    let (owner_id, owner) = fx.user("Owner");
    let (member_id, member) = fx.user("Member");
    let document = fx.document(&owner).await;
    let handle = fx.state.documents.get(document).await.unwrap();
    handle.change_member(owner_id, member_id, Some(Role::Viewer)).await.unwrap();
    let mut subscription = handle.join(member_id).await.unwrap();
    assert!(matches!(subscription.snapshot, ServerMessage::Snapshot { can_edit: false, .. }));
    let edit = fx.link(document, &owner, json!({"role":"editor"})).await;
    let view = fx.link(document, &owner, json!({"role":"viewer"})).await;
    let promoted = fx.call("POST", &join(&edit), Some(&member), Some(json!({}))).await;
    assert_eq!(promoted.status, StatusCode::OK);
    promoted.private();
    assert_eq!(promoted.json()["role"], "editor");
    assert!(promoted.json().get("token").is_none());
    assert!(matches!(subscription.updates.recv().await, Some(ServerMessage::Permissions { can_read: true, can_edit: true, .. })));
    for _ in 0..2 {
        assert_eq!(fx.call("POST", &join(&view), Some(&member), Some(json!({}))).await.json()["role"], "editor");
        assert_eq!(fx.call("POST", &join(&view), Some(&owner), Some(json!({}))).await.json()["role"], "owner");
    }
    assert_eq!(handle.current().await.unwrap().0, 0, "access changes are not document revisions");
    assert_eq!(fx.count("document_members"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_guests_compete_for_one_place_and_the_session_is_document_scoped() {
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let document = fx.document(&owner).await;
    let other = fx.document(&owner).await;
    let link = fx.link(document, &owner, json!({"role":"viewer", "allow_guests":true, "max_guests":1})).await;
    let path = join(&link);
    let (a, b) = tokio::join!(fx.call("POST", &path, None, Some(json!({"name":"Ana ñ"}))), fx.call("POST", &path, None, Some(json!({"name":"Bea"}))));
    let (joined, refused) = if a.status == StatusCode::OK { (a, b) } else { (b, a) };
    assert_eq!(joined.status, StatusCode::OK);
    assert_eq!(refused.status, StatusCode::CONFLICT);
    joined.private();
    let guest = joined.json();
    let token = guest["token"].as_str().unwrap();
    let user = UserId(Uuid::parse_str(guest["user_id"].as_str().unwrap()).unwrap());
    assert_eq!(fx.count("users"), 2);
    assert_eq!(fx.count("guest_accounts"), 1);
    assert_eq!(fx.call("GET", &root(document), Some(&owner), None).await.json()[0]["guests_used"], 1);
    assert_eq!(fx.call("GET", &format!("/api/v1/documents/{}", document.0), Some(token), None).await.json()["role"], "viewer");
    for (method, path, body) in
        [("GET", "/api/v1/users?q=Owner".to_string(), None), ("POST", "/api/v1/documents".to_string(), Some(json!({"title":"not allowed"})))]
    {
        assert_eq!(fx.call(method, &path, Some(token), body).await.status, StatusCode::FORBIDDEN);
    }
    let other_link = fx.link(other, &owner, json!({"role":"editor"})).await;
    // Even a normal account cannot turn a scoped guest into a general server member.
    assert_eq!(
        fx.call("PUT", &format!("/api/v1/documents/{}/members/{}", other.0, user.0), Some(&owner), Some(json!({"role":"editor"}))).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(fx.call("POST", &join(&other_link), Some(token), Some(json!({}))).await.status, StatusCode::FORBIDDEN);
    assert_eq!(fx.call("GET", &format!("/api/v1/documents/{}", other.0), Some(token), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(fx.call("GET", "/api/v1/documents", Some(token), None).await.json().as_array().unwrap().len(), 1);
    // The same authorization is enforced by the actor used by WebSocket.
    let handle = fx.state.documents.get(document).await.unwrap();
    assert!(matches!(handle.join(user).await.unwrap().snapshot, ServerMessage::Snapshot { can_edit: false, .. }));
    let block = handle.current().await.unwrap().1.ids().ids()[0];
    let rejected = handle
        .submit(Submission {
            user,
            client_id: ClientId::new("guest").unwrap(),
            operation_id: OperationId(auth::new_uuid().unwrap()),
            base_revision: 0,
            operation: DocumentOperation::InsertText { block_id: block, offset: 0, text: "not allowed".into() },
        })
        .await
        .unwrap_err();
    assert_eq!(rejected.reason, RejectReason::Forbidden);
    assert_eq!(fx.count("documents"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_revoked_and_invented_tokens_are_indistinguishable_and_do_not_revoke_members() {
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let (_, member) = fx.user("Member");
    let document = fx.document(&owner).await;
    let expired = fx.link(document, &owner, json!({"role":"viewer"})).await;
    let revoked = fx.link(document, &owner, json!({"role":"viewer"})).await;
    assert_eq!(fx.call("POST", &join(&revoked), Some(&member), Some(json!({}))).await.status, StatusCode::OK);
    fx.state
        .db
        .lock()
        .unwrap()
        .execute("UPDATE share_links SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = ?1", [expired["id"].as_str().unwrap()])
        .unwrap();
    fx.call("DELETE", &format!("{}/{}", root(document), revoked["id"].as_str().unwrap()), Some(&owner), None).await;
    let invented = json!({"token":format!("wcs_{}", "0".repeat(64))});
    let mut responses = Vec::new();
    for link in [&expired, &revoked, &invented] {
        let response = fx.call("POST", &join(link), Some(&member), Some(json!({}))).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        responses.push(response.text);
        assert_eq!(fx.call("GET", &format!("/s/{}", link["token"].as_str().unwrap()), None, None).await.status, StatusCode::NOT_FOUND);
    }
    assert!(responses.windows(2).all(|v| v[0] == v[1]));
    assert_eq!(fx.call("GET", &format!("/api/v1/documents/{}", document.0), Some(&member), None).await.status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn validation_and_page_visits_never_create_guests_or_consume_places() {
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let document = fx.document(&owner).await;
    for body in [
        json!({"role":"viewer", "expires_in_seconds":0}),
        json!({"role":"viewer", "expires_in_seconds":links::MAX_LIFETIME_SECONDS+1}),
        json!({"role":"editor", "allow_guests":true, "max_guests":101}),
        json!({"role":"viewer", "max_guests":1}),
    ] {
        assert_eq!(fx.call("POST", &root(document), Some(&owner), Some(body)).await.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(fx.call("POST", &root(document), Some(&owner), Some(json!({"role":"owner"}))).await.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(fx.count("share_links"), 0);
    let link = fx.link(document, &owner, json!({"role":"editor", "allow_guests":true})).await;
    for name in ["".to_string(), "   ".to_string(), "x".repeat(101), "A\0B".to_string()] {
        assert_eq!(fx.call("POST", &join(&link), None, Some(json!({"name":name}))).await.status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(fx.call("POST", &join(&link), Some("invalid"), Some(json!({"name":"Valid name"}))).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(fx.call("POST", &join(&link), None, Some(json!({"name":"x".repeat(20000)}))).await.status, StatusCode::PAYLOAD_TOO_LARGE);
    let page = fx.call("GET", link["path"].as_str().unwrap(), None, None).await;
    assert_eq!(page.status, StatusCode::OK);
    page.private();
    assert!(page.headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap().contains("default-src 'none'"));
    assert!(!page.text.contains("Private plan") && !page.text.contains(link["token"].as_str().unwrap()));
    assert_eq!(fx.count("users"), 1);
    assert_eq!(fx.count("user_tokens"), 1);
    assert_eq!(fx.call("GET", &root(document), Some(&owner), None).await.json()[0]["guests_used"], 0);
    let account_only = fx.link(document, &owner, json!({"role":"viewer"})).await;
    assert_eq!(fx.call("POST", &join(&account_only), None, Some(json!({"name":"Guest"}))).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_guest_enrollment_rolls_back_accounts_tokens_audit_membership_and_quota() {
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let document = fx.document(&owner).await;
    let link = fx.link(document, &owner, json!({"role":"editor", "allow_guests":true, "max_guests":1})).await;
    let audited = fx.count("audit_log");
    fx.state
        .db
        .lock()
        .unwrap()
        .execute_batch("CREATE TRIGGER refuse_enrollment BEFORE INSERT ON document_members BEGIN SELECT RAISE(ABORT, 'test storage failure'); END;")
        .unwrap();
    assert_eq!(fx.call("POST", &join(&link), None, Some(json!({"name":"Guest"}))).await.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        (fx.count("users"), fx.count("user_tokens"), fx.count("guest_accounts"), fx.count("document_members"), fx.count("audit_log")),
        (1, 1, 0, 1, audited)
    );
    assert_eq!(fx.call("GET", &root(document), Some(&owner), None).await.json()[0]["guests_used"], 0);
    fx.state.db.lock().unwrap().execute_batch("DROP TRIGGER refuse_enrollment").unwrap();
    let joined = fx.call("POST", &join(&link), None, Some(json!({"name":"Guest"}))).await;
    assert_eq!(joined.status, StatusCode::OK);
    let token = joined.json()["token"].as_str().unwrap().to_string();
    let handle = fx.state.documents.get(document).await.unwrap();
    let user = auth::authenticate(&fx.state.db, None, Some(&format!("Bearer {token}"))).unwrap().unwrap();
    assert!(matches!(handle.join(user).await.unwrap().snapshot, ServerMessage::Snapshot { can_edit: true, .. }));
}

#[tokio::test(flavor = "multi_thread")]
async fn invitations_quotas_and_guest_sessions_survive_backup_and_restore() {
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let document = fx.document(&owner).await;
    let link = fx.link(document, &owner, json!({"role":"viewer", "allow_guests":true, "max_guests":2})).await;
    let revoked = fx.link(document, &owner, json!({"role":"editor"})).await;
    fx.call("DELETE", &format!("{}/{}", root(document), revoked["id"].as_str().unwrap()), Some(&owner), None).await;
    let guest = fx.call("POST", &join(&link), None, Some(json!({"name":"Guest"}))).await.json();
    let guest_token = guest["token"].as_str().unwrap();
    let key = age::x25519::Identity::generate();
    let settings = backup::BackupConfig { dir: fx._dir.path().join("backups"), recipients: vec![key.to_public()], keep: 1 };
    let summary = backup::create(&fx.config, &settings).unwrap();
    let restored = fx._dir.path().join("restored");
    backup::restore(&summary.path, &[Box::new(key)], &restored).unwrap();
    let state = AppState::open(&config_in(&restored)).unwrap();
    assert_eq!(call(&state, "GET", &format!("/api/v1/documents/{}", document.0), Some(guest_token), None).await.status, StatusCode::OK);
    let listing = call(&state, "GET", &root(document), Some(&owner), None).await.json();
    assert_eq!(listing.as_array().unwrap().len(), 2);
    let live = listing.as_array().unwrap().iter().find(|v| v["id"] == link["id"]).unwrap();
    assert_eq!(live["guests_used"], 1);
    assert_eq!(call(&state, "POST", &join(&revoked), Some(&owner), Some(json!({}))).await.status, StatusCode::NOT_FOUND);
    assert_eq!(call(&state, "POST", &join(&link), None, Some(json!({"name":"Second guest"}))).await.status, StatusCode::OK);
    assert_eq!(call(&state, "POST", &join(&link), None, Some(json!({"name":"One too many"}))).await.status, StatusCode::CONFLICT);
    assert_eq!(
        repository::documents_for(&state.db.lock().unwrap(), UserId(Uuid::parse_str(guest["user_id"].as_str().unwrap()).unwrap())).unwrap().len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_tokens_work_with_the_real_websocket_client_and_keep_their_role() {
    use wordcraft_collab_client::connection::{self, TransportConfig};
    use wordcraft_collab_client::{Event, SharedSession};
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let document = fx.document(&owner).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/api/v1/ws", listener.local_addr().unwrap());
    let state = fx.state.clone();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(wordcraft_collab_server::serve(listener, state, async {
        let _ = stopped.await;
    }));
    for role in ["viewer", "editor"] {
        let link = fx.link(document, &owner, json!({"role":role, "allow_guests":true, "max_guests":1})).await;
        let enrolled = fx.call("POST", &join(&link), None, Some(json!({"name":"Guest"}))).await.json();
        let token = enrolled["token"].as_str().unwrap();
        let mut client = connection::spawn(TransportConfig::new(&url, token), SharedSession::new(document, ClientId::new(role).unwrap()));
        let block = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(Event::Snapshot { replica, .. }) = client.events.recv().await {
                    break replica.ids().ids()[0];
                }
            }
        })
        .await
        .unwrap();
        let result = client
            .submit(OperationId(auth::new_uuid().unwrap()), DocumentOperation::InsertText { block_id: block, offset: 0, text: "ñ📝".into() })
            .await;
        if role == "viewer" {
            assert!(result.is_err(), "the guest viewer may not submit");
        } else {
            result.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(Event::Applied { own: true, revision: 1, .. }) = client.events.recv().await {
                        break;
                    }
                }
            })
            .await
            .unwrap();
        }
        client.close().await;
    }
    assert_eq!(fx.state.documents.get(document).await.unwrap().current().await.unwrap().0, 1);
    let _ = stop.send(());
    tokio::time::timeout(std::time::Duration::from_secs(5), server).await.unwrap().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn resolving_a_link_does_not_bypass_a_later_revocation_or_the_creation_limit() {
    let fx = Fixture::new();
    let (_, owner) = fx.user("Owner");
    let (member_id, _) = fx.user("Member");
    let document = fx.document(&owner).await;
    let link = fx.link(document, &owner, json!({"role":"editor"})).await;
    let hash = links::token_hash(link["token"].as_str().unwrap()).unwrap();
    let routed = links::document_for(&fx.state.db.lock().unwrap(), &hash, &wordcraft_collab_server::persistence::Timestamp::now()).unwrap();
    assert_eq!(routed, document);
    fx.call("DELETE", &format!("{}/{}", root(document), link["id"].as_str().unwrap()), Some(&owner), None).await;
    // A request already routed to the actor must recheck the token before granting access.
    let handle = fx.state.documents.get(document).await.unwrap();
    assert!(matches!(handle.accept_link(hash, links::Applicant::Account(member_id)).await, Err(links::LinkError::NotFound)));
    assert_eq!(handle.access(member_id).await.unwrap_err().reason, RejectReason::Forbidden);
    for _ in 1..links::MAX_LINKS {
        fx.link(document, &owner, json!({"role":"viewer"})).await;
    }
    assert_eq!(fx.call("POST", &root(document), Some(&owner), Some(json!({"role":"viewer"}))).await.status, StatusCode::CONFLICT);
    assert_eq!(fx.count("share_links"), links::MAX_LINKS);
}
