//! C2.5: the document HTTP API through the real router (no socket): create, import, list,
//! metadata, export, history and members, each checked per document.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
use wordcraft_collab_protocol::{ClientId, DocumentId, DocumentOperation, OperationId};
use wordcraft_collab_server::documents::Submission;
use wordcraft_collab_server::persistence::UserId;
use wordcraft_collab_server::{AppState, Config, api, auth};
use wordcraft_doc::{CharProps, Document, Pos};

const DOCX: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

struct Api {
    state: AppState,
    dir: std::path::PathBuf,
}

impl Drop for Api {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Api {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("wordcraft-collab-http-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config =
            Config { listen_addr: "127.0.0.1:0".parse().unwrap(), database_path: dir.join("db"), document_storage: dir.join("docs"), oidc: None };
        Api { state: AppState::open(&config).unwrap(), dir }
    }

    fn user(&self, name: &str) -> (UserId, String) {
        auth::create_local_user(&self.state.db, name).unwrap()
    }

    async fn call(
        &self,
        method: &str,
        uri: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Vec<u8>, Option<String>) {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(ct) = content_type {
            request = request.header(header::CONTENT_TYPE, ct);
        }
        let response = api::router(self.state.clone()).oneshot(request.body(Body::from(body)).unwrap()).await.unwrap();
        let status = response.status();
        let disposition = response.headers().get(header::CONTENT_DISPOSITION).map(|v| v.to_str().unwrap().to_owned());
        (status, to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec(), disposition)
    }

    async fn json(&self, method: &str, uri: &str, token: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (ct, bytes) = match body {
            Some(b) => (Some("application/json"), serde_json::to_vec(&b).unwrap()),
            None => (None, Vec::new()),
        };
        let (status, bytes, _) = self.call(method, uri, Some(token), ct, bytes).await;
        (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn create_list_and_read_metadata() {
    let api = Api::new("create");
    let (ana, ana_token) = api.user("Ana");
    let (_, bea_token) = api.user("Bea");
    let (status, created) = api.json("POST", "/api/v1/documents", &ana_token, Some(json!({"title": "  Plan  "}))).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!((created["title"].as_str(), created["role"].as_str(), created["revision"].as_u64()), (Some("Plan"), Some("owner"), Some(0)));
    assert_eq!(created["owner"].as_str(), Some(ana.0.to_string().as_str()));
    let id = created["id"].as_str().unwrap().to_owned();

    let (status, list) = api.json("GET", "/api/v1/documents", &ana_token, None).await;
    assert_eq!((status, list.as_array().unwrap().len()), (StatusCode::OK, 1));
    let (_, list) = api.json("GET", "/api/v1/documents", &bea_token, None).await;
    assert!(list.as_array().unwrap().is_empty());

    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}"), &ana_token, None).await.1["title"], "Plan");
    // Bea isn't a member: the same 404 as a document that doesn't exist, or a malformed id.
    for uri in [format!("/api/v1/documents/{id}"), format!("/api/v1/documents/{}", Uuid::from_u128(5)), "/api/v1/documents/nope".into()] {
        assert_eq!(api.json("GET", &uri, &bea_token, None).await.0, StatusCode::NOT_FOUND, "{uri}");
    }

    // Bad requests.
    for body in [json!({"title": ""}), json!({"title": "x".repeat(500)}), json!({"name": "x"}), json!({"title": 3})] {
        assert_eq!(api.json("POST", "/api/v1/documents", &ana_token, Some(body.clone())).await.0, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(
        api.call("POST", "/api/v1/documents", Some(&ana_token), Some("text/plain"), b"hi".to_vec()).await.0,
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    // No sign-in, no API.
    assert_eq!(api.call("GET", "/api/v1/documents", None, None, Vec::new()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(api.call("GET", "/api/v1/documents", Some("wct_nope"), None, Vec::new()).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test(flavor = "multi_thread")]
async fn import_edit_and_export_docx() {
    let api = Api::new("docx");
    let (ana, token) = api.user("Ana");
    let mut doc = Document::default();
    doc.insert_text(&Pos::body(0, 0), "Hola ñandú", &CharProps::default()).unwrap();
    doc.split_paragraph(&Pos::body(0, 4)).unwrap();
    let docx = wordcraft_docx::write(&doc).unwrap();
    let (status, body, _) = api.call("POST", "/api/v1/documents?title=Imported%20plan", Some(&token), Some(DOCX), docx).await;
    assert_eq!(status, StatusCode::CREATED);
    let created: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(created["title"], "Imported plan");
    let id = DocumentId(Uuid::parse_str(created["id"].as_str().unwrap()).unwrap());

    // An edit through the actor, then export: the DOCX holds the shared state.
    let handle = api.state.documents.get(id).await.unwrap();
    let block = handle.current().await.unwrap().1.ids().ids()[1];
    let submission = Submission {
        user: ana,
        client_id: ClientId::new("desktop-a").unwrap(),
        operation_id: OperationId(Uuid::from_u128(1)),
        base_revision: 0,
        operation: DocumentOperation::InsertText { block_id: block, offset: 0, text: "¡".into() },
    };
    handle.submit(submission).await.unwrap();
    let (status, bytes, disposition) = api.call("GET", &format!("/api/v1/documents/{}/export", id.0), Some(&token), None, Vec::new()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(disposition.as_deref(), Some("attachment; filename=\"Imported plan.docx\""));
    let exported = wordcraft_docx::read(&bytes).unwrap();
    let texts: Vec<String> = exported
        .body
        .iter()
        .filter_map(|b| match &**b {
            wordcraft_doc::Block::Para(p) => Some(p.text.clone()),
            wordcraft_doc::Block::Table(_) => None,
        })
        .collect();
    assert_eq!(texts, ["Hola", "¡ ñandú"]);

    // The history lists the commit without its text.
    let (status, history) = api.json("GET", &format!("/api/v1/documents/{}/history", id.0), &token, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        history,
        json!([{"revision": 1, "operation_id": Uuid::from_u128(1), "author": ana.0, "client_id": "desktop-a", "kind": "insert_text"}])
    );
    assert!(api.json("GET", &format!("/api/v1/documents/{}/history?after=1", id.0), &token, None).await.1.as_array().unwrap().is_empty());

    // Not a DOCX: refused, nothing created.
    let (status, _, _) = api.call("POST", "/api/v1/documents", Some(&token), Some(DOCX), b"PK\x03\x04 not really".to_vec()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(api.json("GET", "/api/v1/documents", &token, None).await.1.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_owner_manages_members() {
    let api = Api::new("members");
    let (_, ana) = api.user("Ana");
    let (bea_id, bea) = api.user("Bea");
    let (vic_id, vic) = api.user("Vic");
    let id = api.json("POST", "/api/v1/documents", &ana, Some(json!({"title": "Plan"}))).await.1["id"].as_str().unwrap().to_owned();
    let member = |u: UserId| format!("/api/v1/documents/{id}/members/{}", u.0);

    assert_eq!(api.json("PUT", &member(bea_id), &ana, Some(json!({"role": "editor"}))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}"), &bea, None).await.1["role"], "editor");
    // An editor can't manage members; a stranger gets the uniform 404.
    assert_eq!(api.json("PUT", &member(vic_id), &bea, Some(json!({"role": "viewer"}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(api.json("PUT", &member(vic_id), &vic, Some(json!({"role": "editor"}))).await.0, StatusCode::NOT_FOUND);
    // No owners by PUT, no unknown roles or users.
    assert_eq!(api.json("PUT", &member(bea_id), &ana, Some(json!({"role": "owner"}))).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(api.json("PUT", &member(UserId(Uuid::from_u128(9))), &ana, Some(json!({"role": "viewer"}))).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.json("PUT", &member(vic_id), &ana, Some(json!({"role": "viewer"}))).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}"), &vic, None).await.1["role"], "viewer");

    assert_eq!(api.json("DELETE", &member(vic_id), &ana, None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}"), &vic, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.json("DELETE", &member(vic_id), &ana, None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_document_route_rechecks_membership_and_revoked_users_disappear() {
    let api = Api::new("permission-matrix");
    let (owner_id, owner) = api.user("Owner");
    let (editor_id, editor) = api.user("Editor");
    let (viewer_id, viewer) = api.user("Viewer");
    let (_, stranger) = api.user("Stranger");
    let id = api.json("POST", "/api/v1/documents", &owner, Some(json!({"title": "Private"}))).await.1["id"].as_str().unwrap().to_owned();
    let root = format!("/api/v1/documents/{id}");
    for (user, role) in [(editor_id, "editor"), (viewer_id, "viewer")] {
        assert_eq!(api.json("PUT", &format!("{root}/members/{}", user.0), &owner, Some(json!({"role": role}))).await.0, StatusCode::NO_CONTENT);
    }
    for suffix in ["", "/export", "/history"] {
        let uri = format!("{root}{suffix}");
        for token in [&owner, &editor, &viewer] {
            assert_eq!(api.call("GET", &uri, Some(token), None, Vec::new()).await.0, StatusCode::OK, "{suffix}");
        }
        let inaccessible = api.call("GET", &uri, Some(&stranger), None, Vec::new()).await;
        let absent = api.call("GET", &format!("/api/v1/documents/{}{suffix}", Uuid::nil()), Some(&stranger), None, Vec::new()).await;
        assert_eq!((inaccessible.0, inaccessible.1), (absent.0, absent.1), "ids cannot be probed through {suffix}");
        assert_eq!(api.call("GET", &uri, None, None, Vec::new()).await.0, StatusCode::UNAUTHORIZED);
    }
    for token in [&editor, &viewer] {
        assert_eq!(api.json("DELETE", &format!("{root}/members/{}", owner_id.0), token, None).await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            api.json("PUT", &format!("{root}/members/{}", editor_id.0), token, Some(json!({"role": "editor"}))).await.0,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(api.json("DELETE", &format!("{root}/members/{}", viewer_id.0), &owner, None).await.0, StatusCode::NO_CONTENT);
    for suffix in ["", "/export", "/history"] {
        assert_eq!(api.call("GET", &format!("{root}{suffix}"), Some(&viewer), None, Vec::new()).await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(api.json("GET", "/api/v1/documents", &viewer, None).await.1, json!([]));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_reads_an_audit_trail_without_content() {
    let api = Api::new("audit");
    let (ana_id, ana) = api.user("Ana");
    let (bea_id, bea) = api.user("Bea");
    let (_, vic) = api.user("Vic");
    let id = api.json("POST", "/api/v1/documents", &ana, Some(json!({"title": "Secret plans"}))).await.1["id"].as_str().unwrap().to_owned();
    let member = format!("/api/v1/documents/{id}/members/{}", bea_id.0);
    api.json("PUT", &member, &ana, Some(json!({"role": "editor"}))).await;
    assert_eq!(api.call("GET", &format!("/api/v1/documents/{id}/export"), Some(&ana), None, Vec::new()).await.0, StatusCode::OK);
    api.json("DELETE", &member, &ana, None).await;

    let (status, trail) = api.json("GET", &format!("/api/v1/documents/{id}/audit"), &ana, None).await;
    assert_eq!(status, StatusCode::OK);
    let actions: Vec<(&str, &str)> =
        trail.as_array().unwrap().iter().map(|r| (r["action"].as_str().unwrap(), r["detail"].as_str().unwrap())).collect();
    assert_eq!(actions, [("document_created", ""), ("member_set", "editor"), ("document_exported", "docx"), ("member_removed", "")]);
    assert!(trail.as_array().unwrap().iter().all(|r| r["actor"] == json!(ana_id.0)));
    assert_eq!(trail[1]["subject"], json!(bea_id.0));
    assert!(!trail.to_string().contains("Secret"), "the audit trail never holds content");
    // Paging.
    let after = trail[1]["id"].as_i64().unwrap();
    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}/audit?after={after}&limit=1"), &ana, None).await.1.as_array().unwrap().len(), 1);
    // Only the owner: a former member and a stranger get nothing.
    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}/audit"), &bea, None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(api.json("GET", &format!("/api/v1/documents/{id}/audit"), &vic, None).await.0, StatusCode::NOT_FOUND);
}
