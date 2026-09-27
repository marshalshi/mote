use anyhow::{Context, Result};
use base64::Engine;
use futures::{SinkExt, StreamExt};
use marshaling_protocol::{
    AudioClientEvent, AudioServerEvent, AudioStartConfig, ChatRequest, CompactRequest,
    CompactResponse, ModelInfo, RollbackResultPayload, ServerEvent, SessionInfo, UiConfig,
};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

type WsWriter = futures::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    Message,
>;

/// A live WebSocket chat stream. Dropping this sends a close frame to the server.
pub struct ChatStream {
    pub rx: mpsc::UnboundedReceiver<marshaling_protocol::ServerEvent>,
    // Hold the write half so the socket stays fully open. On drop the WS closes.
    _write: WsWriter,
}

pub struct AudioTranscriptionStream {
    pub rx: mpsc::UnboundedReceiver<AudioServerEvent>,
    audio_tx: mpsc::UnboundedSender<Vec<u8>>,
    control_tx: mpsc::UnboundedSender<AudioClientEvent>,
}

impl AudioTranscriptionStream {
    pub fn stop_with_audio(&self, audio: Vec<u8>) {
        let _ = self.audio_tx.send(audio);
        let _ = self.control_tx.send(AudioClientEvent::Stop);
    }

    pub fn cancel(&self) {
        let _ = self.control_tx.send(AudioClientEvent::Cancel);
    }
}

impl ChatStream {
    /// Send a client event (e.g., permission response) over the WebSocket.
    pub async fn send(&mut self, event: marshaling_protocol::ClientEvent) -> Result<()> {
        let json = serde_json::to_string(&event)?;
        self._write.send(Message::Text(json)).await?;
        Ok(())
    }
}

fn websocket_url_from_base(base_url: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(base_url).with_context(|| format!("Invalid server URL: {base_url}"))?;
    if url.scheme() != "http" {
        anyhow::bail!(
            "Only http:// server URLs are supported (got: {})",
            url.scheme()
        );
    }
    // Convert http://host:port → ws://host:port
    let ws = format!("ws://{}/chat", url.authority());
    Ok(ws)
}

fn websocket_url_from_base_with_path(base_url: &str, path: &str) -> Result<String> {
    let url =
        reqwest::Url::parse(base_url).with_context(|| format!("Invalid server URL: {base_url}"))?;
    if url.scheme() != "http" {
        anyhow::bail!(
            "Only http:// server URLs are supported (got: {})",
            url.scheme()
        );
    }
    Ok(format!("ws://{}{}", url.authority(), path))
}

/// Client for communicating with the mote-server.
#[derive(Clone)]
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
        match self
            .http
            .get(format!("{}/health", self.base_url))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
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
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server returned {status}: {body}");
        }
        Ok(resp.json().await?)
    }

    pub async fn list_sessions(&self, runtime_session_key: &str) -> Result<Vec<SessionInfo>> {
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
    pub async fn rollback_last(&self, runtime_session_key: &str) -> Result<RollbackResultPayload> {
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

    pub async fn compact(&self, request: &CompactRequest) -> Result<CompactResponse> {
        let resp = self
            .http
            .post(format!("{}/compact", self.base_url))
            .json(request)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Server returned {status}: {body}");
        }
        Ok(resp.json().await?)
    }

    // ── Credential save ───────────────────────────────────

    /// Save a credential (api_key, token) to the server's auth.json.
    pub async fn save_credential(&self, provider: &str, key: &str, value: &str) -> Result<()> {
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
    pub async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream> {
        let ws_url = websocket_url_from_base(&self.base_url)?;

        let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .context("Failed to connect to server WebSocket")?;

        let (write, mut read) = ws_stream.split();

        // Send the initial request
        let req_json =
            serde_json::to_string(&request).context("Failed to serialize ChatRequest")?;
        let mut write = write;
        if let Err(e) = write.send(Message::Text(req_json)).await {
            anyhow::bail!("Failed to send chat request: {e}");
        }

        let (tx, rx) = mpsc::unbounded_channel();

        // Spawn a task to read events from the WebSocket and forward to the channel
        //
        // Transport loss before a terminal event is deliberately not turned
        // into an error: dropping `tx` closes the channel, which the TUI
        // treats as "detached" and reattaches to the still-running server
        // run. Callers that cannot reattach must treat a closed channel
        // without a terminal event as a failure.
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(msg) => match msg {
                        Message::Text(text) => {
                            match serde_json::from_str::<ServerEvent>(&text) {
                                Ok(event) => {
                                    let is_terminal = matches!(
                                        event,
                                        ServerEvent::Done { .. }
                                            | ServerEvent::Cancelled { .. }
                                            | ServerEvent::NeedsContinuation { .. }
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
                    },
                    Err(e) => {
                        tracing::warn!("Chat websocket read error: {e}");
                        break;
                    }
                }
            }
        });

        Ok(ChatStream { rx, _write: write })
    }

    pub async fn audio_transcription_stream(
        &self,
        config: AudioStartConfig,
    ) -> Result<AudioTranscriptionStream> {
        let ws_url = websocket_url_from_base_with_path(&self.base_url, "/audio/transcribe")?;
        let (ws_stream, _response) = tokio_tungstenite::connect_async(&ws_url)
            .await
            .context("Failed to connect to audio transcription WebSocket")?;
        tracing::debug!(url = %ws_url, "connected audio transcription websocket");
        let (mut write, mut read) = ws_stream.split();
        let start = serde_json::to_string(&AudioClientEvent::Start { config })?;
        write.send(Message::Text(start)).await?;

        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (audio_tx, mut audio_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel::<AudioClientEvent>();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(bytes) = audio_rx.recv() => {
                        tracing::trace!(bytes = bytes.len(), "sending audio chunk");
                        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                        let event = AudioClientEvent::AudioChunk { data };
                        match serde_json::to_string(&event) {
                            Ok(json) => {
                                if write.send(Message::Text(json)).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    Some(event) = control_rx.recv() => {
                        let should_close = matches!(event, AudioClientEvent::Cancel);
                        tracing::debug!(?event, "sending audio control event");
                        match serde_json::to_string(&event) {
                            Ok(json) => {
                                if write.send(Message::Text(json)).await.is_err() {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                        if should_close {
                            break;
                        }
                    }
                    else => break,
                }
            }
        });

        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<AudioServerEvent>(&text) {
                            Ok(event) => {
                                tracing::debug!(?event, "received audio server event");
                                let terminal = matches!(
                                    event,
                                    AudioServerEvent::Stopped | AudioServerEvent::Error { .. }
                                );
                                if event_tx.send(event).is_err() || terminal {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = event_tx.send(AudioServerEvent::Error {
                                    message: format!("Audio protocol error: {e}"),
                                });
                                break;
                            }
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        let reason = frame
                            .map(|f| format!("code={} reason={}", f.code, f.reason))
                            .unwrap_or_else(|| "no close frame".into());
                        tracing::warn!(%reason, "audio websocket closed by server");
                        let _ = event_tx.send(AudioServerEvent::Error {
                            message: format!(
                                "Audio transcription websocket closed by server ({reason})"
                            ),
                        });
                        break;
                    }
                    Err(e) => {
                        tracing::warn!("audio websocket read error: {e}");
                        let _ = event_tx.send(AudioServerEvent::Error {
                            message: format!("Audio websocket read error: {e}"),
                        });
                        break;
                    }
                    Ok(_) => {}
                }
            }
            tracing::debug!("audio websocket reader task ended");
        });

        Ok(AudioTranscriptionStream {
            rx: event_rx,
            audio_tx,
            control_tx,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::websocket_url_from_base;

    #[test]
    fn websocket_url_from_http_base() {
        let ws = websocket_url_from_base("http://127.0.0.1:9847").unwrap();
        assert_eq!(ws, "ws://127.0.0.1:9847/chat");
    }

    #[test]
    fn websocket_url_rejects_https() {
        let err = websocket_url_from_base("https://example.com").unwrap_err();
        assert!(
            err.to_string()
                .contains("Only http:// server URLs are supported"),
            "got: {err}"
        );
    }

    #[test]
    fn websocket_url_rejects_other_schemes() {
        let err = websocket_url_from_base("ftp://example.com").unwrap_err();
        assert!(
            err.to_string()
                .contains("Only http:// server URLs are supported"),
            "got: {err}"
        );
    }
}
