//! WebSocket transport (design §7, milestones C3.1 and C3.3), behind the non-default `transport`
//! feature so wasm builds are unaffected.
//!
//! [`spawn`] starts a Tokio task that owns a [`SharedSession`] and carries its messages:
//! - `ws://` or `wss://` (rustls with the operating system's certificate store, so a homelab CA
//!   or Tailscale certificates work); the token goes in the `Authorization` header, never in the
//!   URL;
//! - on connecting, whatever [`SharedSession::connected`] says (`hello`, `join`, pending
//!   resends); every frame from the server goes through [`SharedSession::receive`], size-capped
//!   at [`MAX_SERVER_MESSAGE_BYTES`];
//! - when the connection drops, [`SharedSession::disconnected`] and a new attempt after an
//!   exponential backoff (reset once connected).
//!
//! The editor talks to it through [`Client`]: `submit`, `acknowledge_conflict`, `replica`,
//! `close`, and a stream of [`Event`]s. All protocol decisions stay in the sans-I/O core.

#[cfg(feature = "transport")]
pub use native::*;

#[cfg(feature = "transport")]
mod native {
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    use wordcraft_collab_model::SharedDocument;
    use wordcraft_collab_protocol::{DocumentOperation, Envelope, MAX_SERVER_MESSAGE_BYTES, OperationId, Revision, ServerMessage};

    use crate::capture::FollowUp;
    use crate::coordinator::{Event, Output, SharedSession, SubmitError};

    /// Events queued for the editor before the transport waits for it to read them.
    pub const EVENT_QUEUE: usize = 1024;

    /// Where and how to connect.
    #[derive(Clone)]
    pub struct TransportConfig {
        /// `wss://host/api/v1/ws` (or `ws://` on a trusted network or loopback).
        pub url: String,
        /// The bearer token (local token now, OIDC access token with C5.1).
        pub token: String,
        /// Where to get a token before each connection attempt instead of `token`: a signed-in
        /// OIDC session that renews it (`true` when the server refused the last one). Blocking.
        pub token_source: Option<TokenSourceFn>,
        /// First wait after a failed or dropped connection; doubles up to `backoff_max`.
        pub backoff_min: Duration,
        pub backoff_max: Duration,
    }

    /// See [`TransportConfig::token_source`].
    pub type TokenSourceFn = std::sync::Arc<dyn Fn(bool) -> Result<String, String> + Send + Sync>;

    impl TransportConfig {
        pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
            Self {
                url: url.into(),
                token: token.into(),
                token_source: None,
                backoff_min: Duration::from_millis(250),
                backoff_max: Duration::from_secs(30),
            }
        }

        /// Get the token from `source` before every attempt (see [`Self::token_source`]).
        pub fn with_token_source(mut self, source: TokenSourceFn) -> Self {
            self.token_source = Some(source);
            self
        }

        /// Validate before opening a document or starting background work. Credentials belong in
        /// the header only: URL credentials and query strings could otherwise end up in logs.
        pub fn validate(&self) -> Result<(), &'static str> {
            let request = self.url.as_str().into_client_request().map_err(|_| "Enter a ws:// or wss:// server address ending in /ws.")?;
            let uri = request.uri();
            if self.url.len() > 2048
                || !matches!(uri.scheme_str(), Some("ws" | "wss"))
                || uri.host().is_none_or(str::is_empty)
                || uri.authority().is_some_and(|a| a.as_str().contains('@'))
                || uri.query().is_some()
                || self.url.contains('#')
                || !uri.path().ends_with("/ws")
            {
                return Err("Enter a ws:// or wss:// server address ending in /ws, without credentials or a query string.");
            }
            if self.token_source.is_none() && !valid_token(&self.token) {
                return Err("Enter a valid sign-in token.");
            }
            Ok(())
        }
    }

    /// A bearer token that can go in a header: printable ASCII, bounded.
    fn valid_token(token: &str) -> bool {
        !token.is_empty() && token.len() <= 8192 && token.bytes().all(|b| b.is_ascii_graphic())
    }

    impl std::fmt::Debug for TransportConfig {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            // Even an invalid URL may contain a pasted credential.
            f.debug_struct("TransportConfig").field("credentials", &"[redacted]").finish_non_exhaustive()
        }
    }

    #[derive(Debug, thiserror::Error, PartialEq, Eq)]
    pub enum ClientError {
        #[error(transparent)]
        Submit(#[from] SubmitError),
        #[error("the connection task has stopped")]
        Stopped,
    }

    enum Command {
        Submit {
            operation_id: OperationId,
            operation: DocumentOperation,
            reply: oneshot::Sender<Result<(), SubmitError>>,
        },
        /// An edit from [`crate::capture`]; a refusal comes back as [`Event::Refused`].
        SubmitEdit {
            first: (OperationId, DocumentOperation),
            follow_up: Option<(OperationId, FollowUp)>,
        },
        AcknowledgeConflict,
        Presence {
            cursor: Option<wordcraft_collab_protocol::messages::Anchor>,
        },
        Replica {
            reply: oneshot::Sender<Option<(Revision, SharedDocument)>>,
        },
        Close,
    }

    /// The editor's side of a shared document's connection.
    pub struct Client {
        commands: mpsc::Sender<Command>,
        /// What happened: snapshots, applied commits, rejections, status changes.
        pub events: mpsc::Receiver<Event>,
        task: JoinHandle<()>,
    }

    /// Start connecting `session` (a fresh [`SharedSession`]) with `config`. Needs a Tokio
    /// runtime.
    pub fn spawn(config: TransportConfig, session: SharedSession) -> Client {
        let (commands, command_rx) = mpsc::channel(64);
        let (event_tx, events) = mpsc::channel(EVENT_QUEUE);
        let task = tokio::spawn(run(config, session, command_rx, event_tx));
        Client { commands, events, task }
    }

    impl Client {
        /// Submit a local edit made against the current replica; it shows up as an
        /// [`Event::Applied`] once committed.
        pub async fn submit(&self, operation_id: OperationId, operation: DocumentOperation) -> Result<(), ClientError> {
            let (reply, answer) = oneshot::channel();
            self.commands.send(Command::Submit { operation_id, operation, reply }).await.map_err(|_| ClientError::Stopped)?;
            Ok(answer.await.map_err(|_| ClientError::Stopped)??)
        }

        pub async fn acknowledge_conflict(&self) -> Result<(), ClientError> {
            self.commands.send(Command::AcknowledgeConflict).await.map_err(|_| ClientError::Stopped)
        }

        /// The replica and its revision (none before the first snapshot).
        pub async fn replica(&self) -> Result<Option<(Revision, SharedDocument)>, ClientError> {
            let (reply, answer) = oneshot::channel();
            self.commands.send(Command::Replica { reply }).await.map_err(|_| ClientError::Stopped)?;
            answer.await.map_err(|_| ClientError::Stopped)
        }

        /// Close the connection and stop reconnecting.
        pub async fn close(self) {
            let _ = self.commands.send(Command::Close).await;
            let _ = self.task.await;
        }
    }

    /// A [`Client`] for code without an async runtime (the desktop editor's UI thread): the
    /// connection runs on its own thread and Tokio runtime, and every call returns at once.
    pub struct ThreadedClient {
        commands: mpsc::Sender<Command>,
        events: mpsc::Receiver<Event>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    /// [`spawn`] on a thread of its own.
    pub fn spawn_threaded(config: TransportConfig, session: SharedSession) -> std::io::Result<ThreadedClient> {
        config.validate().map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let (commands, command_rx) = mpsc::channel(64);
        let (event_tx, events) = mpsc::channel(EVENT_QUEUE);
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
        let thread = std::thread::Builder::new()
            .name("wordcraft-collab".into())
            .spawn(move || runtime.block_on(run(config, session, command_rx, event_tx)))?;
        Ok(ThreadedClient { commands, events, thread: Some(thread) })
    }

    impl ThreadedClient {
        /// Queue an edit; false if the connection thread is gone or busy (the UI says so).
        pub fn submit_edit(&self, first: (OperationId, DocumentOperation), follow_up: Option<(OperationId, FollowUp)>) -> bool {
            self.commands.try_send(Command::SubmitEdit { first, follow_up }).is_ok()
        }

        pub fn acknowledge_conflict(&self) -> bool {
            self.commands.try_send(Command::AcknowledgeConflict).is_ok()
        }

        /// Tell the others where this client's caret is (the caller limits the rate).
        pub fn presence(&self, cursor: Option<wordcraft_collab_protocol::messages::Anchor>) -> bool {
            self.commands.try_send(Command::Presence { cursor }).is_ok()
        }

        /// The next event, if one is waiting.
        pub fn poll(&mut self) -> Option<Event> {
            self.events.try_recv().ok()
        }
    }

    impl Drop for ThreadedClient {
        /// Close the connection and wait for its thread (which only waits on the network
        /// briefly: the close is sent and the task ends).
        fn drop(&mut self) {
            let _ = self.commands.try_send(Command::Close);
            // Unblock a task waiting for room in the event queue.
            self.events.close();
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// A fresh random id (UUID v4 layout) for operations and new paragraphs.
    pub fn new_uuid() -> Option<uuid::Uuid> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).ok()?;
        Some(uuid::Builder::from_random_bytes(bytes).into_uuid())
    }

    /// The server's HTTP base for a WebSocket URL: `wss://h/api/v1/ws` → `https://h/api/v1`.
    pub fn http_base(ws_url: &str) -> Option<String> {
        let rest = ws_url.strip_suffix("/ws")?;
        if let Some(r) = rest.strip_prefix("wss://") {
            Some(format!("https://{r}"))
        } else {
            rest.strip_prefix("ws://").map(|r| format!("http://{r}"))
        }
    }

    /// Share a document on the server (`POST /api/v1/documents` with its DOCX): the new
    /// document's id. Blocking; call it off the UI thread.
    pub fn publish(ws_url: &str, token: &str, title: &str, docx: &[u8]) -> Result<wordcraft_collab_protocol::DocumentId, String> {
        TransportConfig::new(ws_url, token).validate().map_err(str::to_string)?;
        let base = http_base(ws_url).ok_or_else(|| "Enter a ws:// or wss:// server address ending in /ws.".to_string())?;
        let title = encode_query(title);
        let url = format!("{base}/documents?title={title}");
        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/vnd.openxmlformats-officedocument.wordprocessingml.document")
            .send(docx)
            .map_err(|e| format!("publishing to {base}: {e}"))?;
        let text = response.body_mut().read_to_string().map_err(|e| format!("publishing to {base}: {e}"))?;
        let body: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("publishing to {base}: {e}"))?;
        body.get("id")
            .and_then(serde_json::Value::as_str)
            .and_then(|id| uuid::Uuid::parse_str(id).ok())
            .map(wordcraft_collab_protocol::DocumentId)
            .ok_or_else(|| format!("publishing to {base}: no document id in the answer"))
    }

    fn encode_query(value: &str) -> String {
        value
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { char::from(b).to_string() } else { format!("%{b:02X}") })
            .collect()
    }

    /// How a connection ended.
    enum Ended {
        /// Dropped or refused: try again.
        Lost,
        /// The editor closed it (or went away): stop.
        Closed,
    }

    async fn run(config: TransportConfig, session: SharedSession, commands: mpsc::Receiver<Command>, events: mpsc::Sender<Event>) {
        // Dropping the desktop client closes the receiver. Cancel even a stalled DNS/TLS/WS
        // handshake or socket write, so joining the connection thread cannot freeze the UI.
        tokio::select! {
            () = events.closed() => {}
            () = run_connection(config, session, commands, events.clone()) => {}
        }
    }

    async fn run_connection(config: TransportConfig, mut session: SharedSession, mut commands: mpsc::Receiver<Command>, events: mpsc::Sender<Event>) {
        if let Err(message) = config.validate() {
            let _ = emit(&events, vec![Event::Refused { message: message.to_string() }]).await;
            return;
        }
        let mut backoff = config.backoff_min;
        let mut renew = false;
        loop {
            match connect(&config, renew).await {
                Ok(socket) => {
                    backoff = config.backoff_min;
                    renew = false;
                    if let Ended::Closed = drive(socket, &mut session, &mut commands, &events).await {
                        return;
                    }
                }
                // The token expired or was revoked: renew it on the next attempt (once per
                // refusal, so a revoked sign-in does not spin on the provider).
                Err(ConnectError::Unauthorized) => {
                    log::info!("collaboration: {} refused the token", config.url);
                    renew = config.token_source.is_some();
                    if !renew {
                        let _ = emit(&events, vec![Event::Refused { message: "The server refused the sign-in token.".into() }]).await;
                    }
                }
                Err(ConnectError::Other(e)) => log::info!("collaboration: cannot connect to {}: {e}", config.url),
            }
            if emit(&events, session.disconnected().events).await.is_err() {
                return;
            }
            // Wait before the next attempt, still answering the editor (edits are paused).
            let wait = tokio::time::sleep(backoff);
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    () = &mut wait => break,
                    command = commands.recv() => match command {
                        None | Some(Command::Close) => return,
                        Some(command) => {
                            if let Some(out) = handle_command(command, &mut session) {
                                // Nothing can be sent while disconnected; only events matter.
                                if emit(&events, out.events).await.is_err() {
                                    return;
                                }
                            }
                        }
                    },
                }
            }
            backoff = (backoff * 2).min(config.backoff_max);
        }
    }

    type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

    /// Why an attempt failed: the server refused the token (renew it), or anything else.
    enum ConnectError {
        Unauthorized,
        Other(String),
    }

    /// The token for this attempt: from the source (off the async thread: it may renew over
    /// HTTP) or the configured one.
    async fn current_token(config: &TransportConfig, renew: bool) -> Result<String, String> {
        let Some(source) = config.token_source.clone() else { return Ok(config.token.clone()) };
        let token = tokio::task::spawn_blocking(move || source(renew)).await.map_err(|e| e.to_string())??;
        if valid_token(&token) { Ok(token) } else { Err("the sign-in gave an unusable token".into()) }
    }

    async fn connect(config: &TransportConfig, renew: bool) -> Result<Socket, ConnectError> {
        let token = current_token(config, renew).await.map_err(ConnectError::Other)?;
        connect_with(config, &token).await.map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 401 => ConnectError::Unauthorized,
            e => ConnectError::Other(e.to_string()),
        })
    }

    async fn connect_with(config: &TransportConfig, token: &str) -> Result<Socket, tokio_tungstenite::tungstenite::Error> {
        let mut request = config.url.as_str().into_client_request()?;
        let token = format!("Bearer {token}")
            .parse()
            .map_err(|_| tokio_tungstenite::tungstenite::Error::Url(tokio_tungstenite::tungstenite::error::UrlError::NoHostName))?;
        request.headers_mut().insert("authorization", token);
        let limits = WebSocketConfig::default().max_message_size(Some(MAX_SERVER_MESSAGE_BYTES)).max_frame_size(Some(MAX_SERVER_MESSAGE_BYTES));
        let (socket, _) = tokio_tungstenite::connect_async_with_config(request, Some(limits), false).await?;
        Ok(socket)
    }

    async fn drive(socket: Socket, session: &mut SharedSession, commands: &mut mpsc::Receiver<Command>, events: &mpsc::Sender<Event>) -> Ended {
        let (mut sink, mut stream) = socket.split();
        if flush(&mut sink, events, session.connected()).await.is_err() {
            return Ended::Lost;
        }
        loop {
            tokio::select! {
                frame = stream.next() => match frame {
                    Some(Ok(Message::Text(text))) => match Envelope::<ServerMessage>::decode_with_limit(text.as_str(), MAX_SERVER_MESSAGE_BYTES) {
                        Ok(envelope) => {
                            if flush(&mut sink, events, session.receive(envelope.message)).await.is_err() {
                                return Ended::Lost;
                            }
                        }
                        Err(e) => log::warn!("collaboration: undecodable server message: {e}"),
                    },
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Ended::Lost,
                    Some(Ok(_)) => {}
                },
                command = commands.recv() => match command {
                    None | Some(Command::Close) => {
                        let _ = sink.close().await;
                        return Ended::Closed;
                    }
                    Some(command) => {
                        if let Some(out) = handle_command(command, session)
                            && flush(&mut sink, events, out).await.is_err()
                        {
                            return Ended::Lost;
                        }
                    }
                },
            }
        }
    }

    /// Apply an editor command to the session; what to send and emit, if anything.
    fn handle_command(command: Command, session: &mut SharedSession) -> Option<Output> {
        match command {
            Command::Submit { operation_id, operation, reply } => {
                let result = session.submit(operation_id, operation);
                let out = result.as_ref().ok().map(|message| Output { send: vec![message.clone()], events: Vec::new() });
                let _ = reply.send(result.map(|_| ()));
                out
            }
            Command::SubmitEdit { first, follow_up } => Some(match session.submit_edit(first, follow_up) {
                Ok(message) => Output { send: vec![message], events: Vec::new() },
                Err(e) => Output { send: Vec::new(), events: vec![Event::Refused { message: e.to_string() }] },
            }),
            Command::AcknowledgeConflict => Some(session.acknowledge_conflict()),
            Command::Presence { cursor } => session.presence(cursor).map(|m| Output { send: vec![m], events: Vec::new() }),
            Command::Replica { reply } => {
                let _ = reply.send(session.replica().map(|r| (session.revision(), r.clone())));
                None
            }
            Command::Close => None,
        }
    }

    /// Send `out.send` in order, then hand `out.events` to the editor.
    async fn flush(sink: &mut futures_util::stream::SplitSink<Socket, Message>, events: &mpsc::Sender<Event>, out: Output) -> Result<(), ()> {
        for message in &out.send {
            let text = Envelope::new(message).encode().map_err(|e| log::error!("collaboration: encoding {message:?}: {e}"))?;
            sink.send(Message::Text(text.into())).await.map_err(|_| ())?;
        }
        emit(events, out.events).await
    }

    async fn emit(events: &mpsc::Sender<Event>, list: Vec<Event>) -> Result<(), ()> {
        for event in list {
            events.send(event).await.map_err(|_| ())?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::{SharedSession, TransportConfig, encode_query, spawn_threaded};

        #[test]
        fn dropping_the_desktop_client_cancels_a_stalled_handshake() {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("ws://{}/api/v1/ws", listener.local_addr().unwrap());
            let session = SharedSession::new(
                wordcraft_collab_protocol::DocumentId(uuid::Uuid::nil()),
                wordcraft_collab_protocol::ClientId::new("desktop-test").unwrap(),
            );
            let client = spawn_threaded(TransportConfig::new(url, "test-token"), session).unwrap();
            listener.set_nonblocking(true).unwrap();
            let start = std::time::Instant::now();
            let peer = loop {
                if let Ok((peer, _)) = listener.accept() {
                    break peer;
                }
                assert!(start.elapsed() < std::time::Duration::from_secs(5), "client connected");
                std::thread::yield_now();
            };
            // Keep the socket open without answering the WebSocket handshake.
            let (tx, rx) = std::sync::mpsc::channel();
            let thread = std::thread::spawn(move || {
                drop(client);
                let _ = tx.send(());
            });
            assert!(rx.recv_timeout(std::time::Duration::from_secs(2)).is_ok(), "disconnect must not wait for the server");
            thread.join().unwrap();
            drop(peer);
        }

        #[test]
        fn connection_settings_reject_credentials_in_urls_and_header_injection() {
            for url in ["ws://localhost:3000/api/v1/ws", "wss://homelab.example/api/v1/ws", "wss://[::1]/prefix/ws"] {
                assert!(TransportConfig::new(url, "wct_example").validate().is_ok(), "{url}");
            }
            for url in [
                "",
                "localhost:3000",
                "https://host/api/v1/ws",
                "ws:///ws",
                "wss://host/api/v1",
                "wss://user:secret@host/ws",
                "wss://host/ws?token=secret",
                "wss://host/ws#secret",
            ] {
                let error = TransportConfig::new(url, "wct_example").validate().unwrap_err();
                assert!(!error.contains("secret"));
            }
            for token in ["", " ", "token\r\nAuthorization: injected", "tökén"] {
                assert!(TransportConfig::new("wss://host/ws", token).validate().is_err());
            }
            assert!(!format!("{:?}", TransportConfig::new("wss://secret@host/ws", "secret")).contains("secret"));
        }

        #[test]
        fn publishing_encodes_utf8_and_query_delimiters() {
            assert_eq!(encode_query("Plan ñ — 📝 & + # / ?"), "Plan%20%C3%B1%20%E2%80%94%20%F0%9F%93%9D%20%26%20%2B%20%23%20%2F%20%3F");
            assert_eq!(encode_query("a-Z_0.~"), "a-Z_0.~");
        }
    }
}
