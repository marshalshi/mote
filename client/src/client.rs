use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use marshaling_protocol::{
    ChatRequest, ModelInfo, RollbackResultPayload, ServerEvent, SessionInfo,
    UiConfig,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type WsWriter = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    Message,
>;

/// A live WebSocket chat stream. Dropping this sends a close frame to the server.
pub struct ChatStream {
    pub rx: mpsc::UnboundedReceiver<marshaling_protocol::ServerEvent>,
    // Hold the write half so the socket stays fully open. On drop the WS closes.
    _write: WsWriter,
}

impl ChatStream {
    /// Send a client event (e.g., permission response) over the WebSocket.
    pub async fn send(
        &mut self,
        event: marshaling_protocol::ClientEvent,
    ) -> Result<()> {
        let json = serde_json::to_string(&event)?;
        self._write.send(Message::Text(json.into())).await?;
        Ok(())
    }
}

/// Client for communicating with the mote-server.
pub struct MoteClient {
    base_url: String,
    http: reqwest::Client,
}

impl MoteClient {
    pub fn new(addr: &str) -> Self {
        Self {
            base_url: addr.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub async fn health(&self) -> bool {
        self.http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
            .is_ok()
    }

    pub async fn get_config(&self) -> Result<UiConfig> {
        let resp = self
            .http
            .get(format!("{}/config", self.base_url))
            .send()
            .await?;
        Ok(resp.json().await?)
    }

    pub async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        let resp = self
            .http
            .get(format!("{}/models", self.base_url))
            .send()
            .await?;
        Ok(resp.json().await?)
    }

    pub async fn list_sessions(
        &self,
        runtime_session_key: &str,
    ) -> Result<Vec<SessionInfo>> {
        let resp = self
            .http
            .get(format!("{}/sessions", self.base_url))
            .header("x-mote-session-key", runtime_session_key)
            .send()
            .await?;
        Ok(resp.json().await?)
    }

    /// Load a saved session by ID.
    pub async fn load_session(
        &self,
        runtime_session_key: &str,
        id: &str,
    ) -> Result<marshaling_protocol::SessionData> {
        let resp = self
            .http
            .get(format!("{}/sessions/{id}", self.base_url))
            .header("x-mote-session-key", runtime_session_key)
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Server returned {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    /// Roll back the most recent tracked file mutation set.
    pub async fn rollback_last(
        &self,
        runtime_session_key: &str,
    ) -> Result<RollbackResultPayload> {
        let resp = self
            .http
            .post(format!("{}/rollback/last", self.base_url))
            .json(&marshaling_protocol::RollbackLastRequest {
                runtime_session_key: runtime_session_key.to_string(),
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            anyhow::bail!("Server returned {}", resp.status());
        }
        Ok(resp.json().await?)
    }

    // ── Credential save ───────────────────────────────────

    /// Save a credential (api_key, token) to the server's auth.json.
    pub async fn save_credential(
        &self,
        provider: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        let body = serde_json::json!({
            "provider": provider,
            key: value,
        });
        let resp = self
            .http
            .post(format!("{}/auth/save", self.base_url))
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body: serde_json::Value = resp.json().await.unwrap_or_default();
            let err = body
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("unknown error");
            anyhow::bail!("Failed to save credential ({}): {}", status, err);
        }
        Ok(())
    }

    /// Start a streaming chat session via WebSocket.
    ///
    /// Sends the initial [`ChatRequest`], then returns a [`ChatStream`]
    /// whose `rx` field yields [`ServerEvent`] messages as they arrive.
    /// The stream stays open until dropped.
    pub async fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> Result<ChatStream> {
        let ws_url = format!(
            "ws://{}/chat",
            self.base_url
                .trim_start_matches("http://")
                .trim_start_matches("https://")
        );

        let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .context("Failed to connect to server WebSocket")?;

        let (write, mut read) = ws_stream.split();

        // Send the initial request
        let req_json = serde_json::to_string(&request)
            .context("Failed to serialize ChatRequest")?;
        let mut write = write;
        if let Err(e) = write.send(Message::Text(req_json.into())).await {
            anyhow::bail!("Failed to send chat request: {e}");
        }

        let (tx, rx) = mpsc::unbounded_channel();

        // Spawn a task to read events from the WebSocket and forward to the channel
        tokio::spawn(async move {
            while let Some(Ok(msg)) = read.next().await {
                match msg {
                    Message::Text(text) => {
                        match serde_json::from_str::<ServerEvent>(&text) {
                            Ok(event) => {
                                let is_terminal = matches!(
                                    event,
                                    ServerEvent::Done { .. }
                                        | ServerEvent::Error { .. }
                                );
                                if tx.send(event).is_err() {
                                    break;
                                }
                                if is_terminal {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "Failed to parse server event: {e} — data: {text}"
                                );
                                // Send an error event so the TUI can surface it
                                let _ = tx.send(ServerEvent::Error {
                                    message: format!("Protocol error: {e}"),
                                });
                                break;
                            }
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        });

        Ok(ChatStream { rx, _write: write })
    }
}
