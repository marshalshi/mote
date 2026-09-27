//! End-to-end tests: the real server (router, runs, persistence, provider
//! client) driven over a real WebSocket, against a fake OpenAI-compatible
//! provider that plays back scripted responses and records every request.
//!
//! These cover the seams unit tests cannot: reattaching, the permission
//! round trip, sessions continuing from the stored transcript, retries,
//! compaction, and a server restart.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use marshaling_protocol::{ChatRequest, ClientEvent, ServerEvent};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::*;

// ── Fake provider ─────────────────────────────────────────

/// One scripted provider response.
enum Reply {
    /// A streamed completion: SSE chunks (JSON), then `[DONE]`.
    Stream(Vec<Value>),
    /// A non-streamed completion (used by compaction).
    Completion(String),
    /// An HTTP error, with an optional `retry-after-ms` header.
    Status(u16, String, Option<u64>),
}

#[derive(Default)]
struct FakeProvider {
    replies: Mutex<VecDeque<Reply>>,
    /// Request bodies received, in order.
    requests: Mutex<Vec<Value>>,
}

impl FakeProvider {
    fn push(&self, reply: Reply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

async fn fake_chat(
    State(fake): State<Arc<FakeProvider>>,
    axum::Json(body): axum::Json<Value>,
) -> Response {
    fake.requests.lock().unwrap().push(body);
    let reply = fake.replies.lock().unwrap().pop_front();
    match reply {
        Some(Reply::Stream(chunks)) => {
            let mut sse = String::new();
            for chunk in chunks {
                sse.push_str(&format!("data: {chunk}\n\n"));
            }
            sse.push_str("data: [DONE]\n\n");
            ([("content-type", "text/event-stream")], sse).into_response()
        }
        Some(Reply::Completion(text)) => axum::Json(json!({
            "choices": [{"message": {"content": text}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        }))
        .into_response(),
        Some(Reply::Status(code, body, retry_after_ms)) => {
            let mut response = (
                axum::http::StatusCode::from_u16(code).unwrap(),
                body,
            )
                .into_response();
            if let Some(ms) = retry_after_ms {
                response
                    .headers_mut()
                    .insert("retry-after-ms", ms.to_string().parse().unwrap());
            }
            response
        }
        // A 400 is not retried, so a test with too few scripted replies
        // fails at once instead of after retries and a timeout.
        None => (
            axum::http::StatusCode::BAD_REQUEST,
            "fake provider: script ran out",
        )
            .into_response(),
    }
}

fn text_reply(text: &str) -> Reply {
    Reply::Stream(vec![
        json!({"choices": [{"delta": {"content": text}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}],
               "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}),
    ])
}

fn tool_reply(calls: &[(&str, &str, Value)]) -> Reply {
    let tool_calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({"index": index, "id": id, "type": "function",
                   "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    Reply::Stream(vec![
        json!({"choices": [{"delta": {"tool_calls": tool_calls}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    ])
}

async fn start_fake_provider() -> (Arc<FakeProvider>, String) {
    let fake = Arc::new(FakeProvider::default());
    let app = axum::Router::new()
        .route("/chat/completions", axum::routing::post(fake_chat))
        .with_state(Arc::clone(&fake));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (fake, url)
}

// ── Server under test ─────────────────────────────────────

struct Harness {
    url: String,
    fake: Arc<FakeProvider>,
    /// Keeps history and workspace alive for the test's duration.
    dirs: Arc<(tempfile::TempDir, tempfile::TempDir)>,
}

const CLIENT_KEY: &str = "e2e-key";

impl Harness {
    async fn start() -> Self {
        let (fake, fake_url) = start_fake_provider().await;
        let dirs = Arc::new((
            tempfile::tempdir().unwrap(),
            tempfile::tempdir().unwrap(),
        ));
        std::fs::write(
            dirs.1.path().join("notes.txt"),
            "The secret word is: lighthouse\n",
        )
        .unwrap();
        let url = serve(&fake_url, dirs.0.path()).await;
        Self { url, fake, dirs }
    }

    /// Another server over the same history and provider: sees only what the
    /// first one wrote to disk, like the server after a restart.
    async fn second_server(&self, fake_url: &str) -> Self {
        let url = serve(fake_url, self.dirs.0.path()).await;
        Self {
            url,
            fake: Arc::clone(&self.fake),
            dirs: Arc::clone(&self.dirs),
        }
    }

    fn workspace(&self) -> &std::path::Path {
        self.dirs.1.path()
    }

    fn history(&self) -> std::path::PathBuf {
        self.dirs.0.path().join(CLIENT_KEY)
    }

    fn request(&self, message: &str, session_id: Option<&str>) -> ChatRequest {
        ChatRequest {
            message: message.into(),
            agent: "build".into(),
            model_override: None,
            provider_override: None,
            history: Vec::new(),
            session_id: session_id.map(str::to_string),
            workspace_root: Some(self.workspace().display().to_string()),
            repo_agents_md: None,
            runtime_session_key: Some(CLIENT_KEY.into()),
            run_id: None,
            replay_from: None,
            client_instance_id: Some("e2e-instance".into()),
            compaction: None,
        }
    }

    async fn connect(&self, request: &ChatRequest) -> Ws {
        let ws_url = format!("{}/chat", self.url.replace("http://", "ws://"));
        let (mut ws, _) =
            tokio_tungstenite::connect_async(ws_url).await.unwrap();
        ws.send(WsMessage::Text(serde_json::to_string(request).unwrap()))
            .await
            .unwrap();
        ws
    }
}

async fn serve(fake_url: &str, history_dir: &std::path::Path) -> String {
    let mut config: config::Config = toml::from_str(&format!(
        r#"
[model]
provider = "deepseek"
model_id = "fake-model"
max_tokens = 1024

[providers.deepseek]
base_url = "{fake_url}"
api_key = "test-key"

[server]
default_agent = "build"
first_response_timeout_secs = 10
stream_idle_timeout_secs = 10

[prompts]
default = "/nonexistent/system-prompt.md"

[permissions]
default = "ask"
read = "allow"
glob = "allow"
grep = "allow"
write = "ask"
"#
    ))
    .unwrap();
    config.history.dir = history_dir.to_path_buf();
    // Keep tests independent of the developer's ~/.config/mote/AGENTS.md.
    let build_agent = config::parse_agent_markdown(
        "---\ndisable_user_agents_md: true\n---\n",
    )
    .unwrap();
    let state = Arc::new(AppState {
        config,
        auth: RwLock::new(auth::Auth::default()),
        merged_agents: HashMap::from([("build".to_string(), build_agent)]),
        runtime_states: tokio::sync::Mutex::new(HashMap::new()),
        runs: tokio::sync::Mutex::new(HashMap::new()),
        completed_run_ids: tokio::sync::Mutex::new(VecDeque::new()),
        session_locks: tokio::sync::Mutex::new(HashMap::new()),
        session_info_cache: Arc::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, build_router(state)).await;
    });
    url
}

// ── WebSocket client ──────────────────────────────────────

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

async fn next_event(ws: &mut Ws) -> ServerEvent {
    loop {
        let message = tokio::time::timeout(Duration::from_secs(15), ws.next())
            .await
            .expect("timed out waiting for a server event")
            .expect("socket closed")
            .unwrap();
        if let WsMessage::Text(text) = message {
            return serde_json::from_str(&text).unwrap();
        }
    }
}

fn is_terminal(event: &ServerEvent) -> bool {
    matches!(
        event,
        ServerEvent::Done { .. }
            | ServerEvent::Cancelled { .. }
            | ServerEvent::NeedsContinuation { .. }
            | ServerEvent::Error { .. }
    )
}

/// Read events up to the terminal one, answering permission prompts.
async fn until_terminal(ws: &mut Ws, allow: bool) -> Vec<ServerEvent> {
    let mut events = Vec::new();
    loop {
        let event = next_event(ws).await;
        if let ServerEvent::PermissionRequest { id, .. }
        | ServerEvent::PermissionPending { id, .. } = &event
        {
            answer(ws, id, allow).await;
        }
        let done = is_terminal(&event);
        events.push(event);
        if done {
            return events;
        }
    }
}

async fn answer(ws: &mut Ws, id: &str, allowed: bool) {
    let response = ClientEvent::PermissionResponse {
        id: id.into(),
        allowed,
        remember: false,
    };
    ws.send(WsMessage::Text(serde_json::to_string(&response).unwrap()))
        .await
        .unwrap();
}

fn session_id(events: &[ServerEvent]) -> String {
    events
        .iter()
        .find_map(|e| match e {
            ServerEvent::RunStarted { session_id, .. } => session_id.clone(),
            _ => None,
        })
        .expect("run_started with a session id")
}

/// The messages the server sent to the provider in request `n`.
fn sent_messages(fake: &FakeProvider, n: usize) -> Vec<Value> {
    fake.requests()[n]["messages"].as_array().unwrap().clone()
}

fn mentions(messages: &[Value], needle: &str) -> bool {
    messages.iter().any(|m| m.to_string().contains(needle))
}

// ── Tests ─────────────────────────────────────────────────

#[tokio::test]
async fn tool_turn_is_stored_and_sent_back_on_the_next_turn() {
    let h = Harness::start().await;
    h.fake.push(tool_reply(&[(
        "call_1",
        "read",
        json!({"file_path": "notes.txt"}),
    )]));
    h.fake.push(text_reply("The word is lighthouse."));
    h.fake.push(text_reply("You asked about notes.txt."));

    let mut ws = h
        .connect(&h.request("What is the secret word?", None))
        .await;
    let events = until_terminal(&mut ws, true).await;
    assert!(
        matches!(events.last(), Some(ServerEvent::Done { .. })),
        "{events:?}"
    );
    let session = session_id(&events);

    // The transcript holds the tool call and its result.
    let transcript =
        store::load(&store::transcript_path(&h.history(), &session)).unwrap();
    assert!(transcript.messages.iter().any(|m| {
        m.message.tool_call_id.as_deref() == Some("call_1")
            && m.message
                .content
                .as_deref()
                .is_some_and(|c| c.contains("lighthouse"))
    }));

    // The next turn continues from the transcript, tool history included.
    let mut ws = h
        .connect(&h.request("What did I ask before?", Some(&session)))
        .await;
    let events = until_terminal(&mut ws, true).await;
    assert!(matches!(events.last(), Some(ServerEvent::Done { .. })));
    let sent = sent_messages(&h.fake, 2);
    assert!(
        mentions(&sent, "\"tool_call_id\":\"call_1\""),
        "tool result resent"
    );
    assert!(mentions(&sent, "What is the secret word?"));
    assert!(mentions(&sent, "What did I ask before?"));
}

#[tokio::test]
async fn permission_prompt_survives_a_dropped_connection() {
    let h = Harness::start().await;
    h.fake.push(tool_reply(&[(
        "call_w",
        "write",
        json!({"file_path": "answer.txt", "content": "lighthouse"}),
    )]));
    h.fake.push(text_reply("Written."));

    let request = h.request("Write the answer file.", None);
    let mut ws = h.connect(&request).await;
    // Read up to the prompt, counting run-log events like the TUI does.
    let mut count = 0;
    let mut run_id = None;
    loop {
        let event = next_event(&mut ws).await;
        if event.is_run_log_event() {
            count += 1;
        }
        if let ServerEvent::RunStarted { run_id: id, .. } = &event {
            run_id = Some(id.clone());
        }
        if matches!(event, ServerEvent::PermissionRequest { .. }) {
            break;
        }
    }
    // The connection drops before the user answers.
    drop(ws);

    let mut reattach = h.request("", None);
    reattach.run_id = run_id;
    reattach.replay_from = Some(count);
    let mut ws = h.connect(&reattach).await;
    let events = until_terminal(&mut ws, true).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::PermissionPending { .. })),
        "the unanswered prompt is announced again: {events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ServerEvent::RunStarted { .. })),
        "already-seen events are not replayed"
    );
    assert!(matches!(events.last(), Some(ServerEvent::Done { .. })));
    assert_eq!(
        std::fs::read_to_string(h.workspace().join("answer.txt")).unwrap(),
        "lighthouse"
    );
}

#[tokio::test]
async fn transient_provider_error_is_retried() {
    let h = Harness::start().await;
    h.fake
        .push(Reply::Status(503, "overloaded".into(), Some(0)));
    h.fake.push(text_reply("recovered"));

    let mut ws = h.connect(&h.request("hello", None)).await;
    let events = until_terminal(&mut ws, true).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ServerEvent::Retrying { attempt: 1, .. }))
    );
    assert!(
        matches!(events.last(), Some(ServerEvent::Done { content, .. }) if content == "recovered")
    );
    assert_eq!(h.fake.requests().len(), 2);
}

#[tokio::test]
async fn fatal_provider_error_ends_the_run_and_keeps_the_message() {
    let h = Harness::start().await;
    h.fake.push(Reply::Status(
        400,
        "This model's maximum context length is 8192 tokens".into(),
        None,
    ));

    let mut ws = h.connect(&h.request("a very long question", None)).await;
    let events = until_terminal(&mut ws, true).await;
    assert!(matches!(events.last(), Some(ServerEvent::Error { .. })));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ServerEvent::Retrying { .. }))
    );
    assert_eq!(
        h.fake.requests().len(),
        1,
        "context overflow is not retried"
    );
    let transcript = store::load(&store::transcript_path(
        &h.history(),
        &session_id(&events),
    ))
    .unwrap();
    assert_eq!(
        transcript.messages[0].message.content.as_deref(),
        Some("a very long question")
    );
}

#[tokio::test]
async fn second_request_from_the_same_client_attaches_to_the_active_run() {
    let h = Harness::start().await;
    h.fake.push(tool_reply(&[(
        "call_w",
        "write",
        json!({"file_path": "a.txt", "content": "x"}),
    )]));
    h.fake.push(text_reply("done"));

    let mut first = h.connect(&h.request("write a.txt", None)).await;
    while !matches!(
        next_event(&mut first).await,
        ServerEvent::PermissionRequest { .. }
    ) {}

    let mut second = h.connect(&h.request("another message", None)).await;
    assert!(matches!(
        next_event(&mut second).await,
        ServerEvent::SessionBusy { .. }
    ));
    let events = until_terminal(&mut second, true).await;
    assert!(matches!(events.last(), Some(ServerEvent::Done { .. })));
    // Only the first message ever reached the provider.
    assert_eq!(h.fake.requests().len(), 2);
    assert!(!mentions(&sent_messages(&h.fake, 1), "another message"));
}

#[tokio::test]
async fn session_continues_after_a_server_restart() {
    let (fake, fake_url) = start_fake_provider().await;
    let dirs =
        Arc::new((tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap()));
    let first = Harness {
        url: serve(&fake_url, dirs.0.path()).await,
        fake: Arc::clone(&fake),
        dirs: Arc::clone(&dirs),
    };
    fake.push(text_reply("Nice to meet you, Sam."));
    fake.push(text_reply("Your name is Sam."));

    let mut ws = first.connect(&first.request("My name is Sam.", None)).await;
    let events = until_terminal(&mut ws, true).await;
    let session = session_id(&events);

    let second = first.second_server(&fake_url).await;
    let mut ws = second
        .connect(&second.request("What is my name?", Some(&session)))
        .await;
    let events = until_terminal(&mut ws, true).await;
    assert!(matches!(events.last(), Some(ServerEvent::Done { .. })));
    let sent = sent_messages(&fake, 1);
    assert!(mentions(&sent, "My name is Sam."));
    assert!(mentions(&sent, "Nice to meet you, Sam."));
}

#[tokio::test]
async fn compaction_replaces_older_turns_with_the_summary() {
    let h = Harness::start().await;
    h.fake.push(text_reply("first answer"));
    h.fake
        .push(Reply::Completion("SUMMARY: the user said hello".into()));
    h.fake.push(text_reply("second answer"));

    let mut ws = h.connect(&h.request("hello there ZX-7731", None)).await;
    let session = session_id(&until_terminal(&mut ws, true).await);

    let response: marshaling_protocol::CompactResponse = reqwest::Client::new()
        .post(format!("{}/compact", h.url))
        .json(&json!({
            "agent": "build",
            "model_override": null,
            "history": [],
            "session_id": session,
            "workspace_root": h.workspace().display().to_string(),
            "runtime_session_key": CLIENT_KEY,
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(response.session_id, session);
    assert_eq!(response.compaction.compacted_message_count, 2);

    let mut ws = h.connect(&h.request("next question", Some(&session))).await;
    let events = until_terminal(&mut ws, true).await;
    assert!(matches!(events.last(), Some(ServerEvent::Done { .. })));
    let sent = sent_messages(&h.fake, 2);
    assert!(mentions(&sent, "SUMMARY: the user said hello"));
    assert!(
        !mentions(&sent, "ZX-7731"),
        "compacted turns are not resent"
    );
    assert!(mentions(&sent, "next question"));
}

#[tokio::test]
async fn rollback_is_noted_for_the_model() {
    let h = Harness::start().await;
    h.fake.push(tool_reply(&[(
        "call_w",
        "write",
        json!({"file_path": "made.txt", "content": "new file"}),
    )]));
    h.fake.push(text_reply("Created."));
    h.fake.push(text_reply("Understood."));

    let mut ws = h.connect(&h.request("create made.txt", None)).await;
    let session = session_id(&until_terminal(&mut ws, true).await);
    assert!(h.workspace().join("made.txt").exists());

    let result: marshaling_protocol::RollbackResultPayload =
        reqwest::Client::new()
            .post(format!("{}/rollback/last", h.url))
            .json(&json!({"runtime_session_key": CLIENT_KEY}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert!(result.success, "{}", result.message);
    assert!(!h.workspace().join("made.txt").exists());

    let mut ws = h.connect(&h.request("what now?", Some(&session))).await;
    until_terminal(&mut ws, true).await;
    assert!(mentions(
        &sent_messages(&h.fake, 2),
        "rolled back the changes"
    ));
    // The note is for the model, not the chat view.
    let data: marshaling_protocol::SessionData = reqwest::Client::new()
        .get(format!("{}/sessions/{session}", h.url))
        .header("x-mote-session-key", CLIENT_KEY)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !data
            .messages
            .iter()
            .any(|m| m.content.contains("rolled back"))
    );
}

#[tokio::test]
async fn context_overflow_prunes_old_tool_output_and_retries() {
    let h = Harness::start().await;
    std::fs::write(h.workspace().join("big.txt"), "data ".repeat(200)).unwrap();
    for id in ["c1", "c2", "c3"] {
        h.fake.push(tool_reply(&[(
            id,
            "read",
            json!({"file_path": "big.txt"}),
        )]));
    }
    h.fake.push(Reply::Status(
        400,
        "This model's maximum context length is 8192 tokens".into(),
        None,
    ));
    h.fake.push(text_reply("done after pruning"));

    let mut ws = h
        .connect(&h.request("read big.txt three times", None))
        .await;
    let events = until_terminal(&mut ws, true).await;
    assert!(events.iter().any(|e| matches!(
        e,
        ServerEvent::Retrying { reason, .. } if reason.contains("too long")
    )));
    assert!(
        matches!(events.last(), Some(ServerEvent::Done { content, .. }) if content == "done after pruning"),
        "{events:?}"
    );
    // The retried request clears the oldest result but keeps the recent ones.
    let retried = sent_messages(&h.fake, 4);
    assert!(mentions(&retried, "older tool output cleared"));
    assert!(mentions(&retried, "data data"));
}

#[tokio::test]
async fn rollback_during_a_run_reaches_the_running_model() {
    let h = Harness::start().await;
    h.fake.push(tool_reply(&[(
        "call_a",
        "write",
        json!({"file_path": "a.txt", "content": "first"}),
    )]));
    h.fake.push(tool_reply(&[(
        "call_b",
        "write",
        json!({"file_path": "b.txt", "content": "second"}),
    )]));
    h.fake.push(text_reply("all done"));

    let mut ws = h.connect(&h.request("write two files", None)).await;
    // Approve the first write; stop at the second prompt.
    let mut prompts = 0;
    let second_prompt = loop {
        if let ServerEvent::PermissionRequest { id, .. } =
            next_event(&mut ws).await
        {
            prompts += 1;
            if prompts == 1 {
                answer(&mut ws, &id, true).await;
            } else {
                break id;
            }
        }
    };
    assert!(h.workspace().join("a.txt").exists());

    // While the run waits, the user rolls back the first write.
    let result: marshaling_protocol::RollbackResultPayload =
        reqwest::Client::new()
            .post(format!("{}/rollback/last", h.url))
            .json(&json!({"runtime_session_key": CLIENT_KEY}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
    assert!(result.success, "{}", result.message);
    assert!(!h.workspace().join("a.txt").exists());

    answer(&mut ws, &second_prompt, true).await;
    let events = until_terminal(&mut ws, true).await;
    assert!(
        matches!(events.last(), Some(ServerEvent::Done { .. })),
        "{events:?}"
    );
    // The running model is told before its next step.
    assert!(mentions(
        &sent_messages(&h.fake, 2),
        "rolled back the changes"
    ));
}
