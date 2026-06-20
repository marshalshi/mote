use crate::config::Config;
use crate::llm::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Global counter for generating unique Ollama tool call IDs.
static OLLAMA_CALL_ID: AtomicU64 = AtomicU64::new(0);

fn request_item_count(body: &serde_json::Value, key: &str) -> usize {
    body.get(key).and_then(|v| v.as_array()).map_or(0, Vec::len)
}

#[derive(Clone)]
pub struct OllamaProvider {
    base_url: String,
    client: reqwest::Client,
}

impl OllamaProvider {
    pub fn new(config: &Config, _auth: &crate::auth::Auth) -> Result<Self> {
        Ok(Self {
            base_url: config.ollama_base_url()?,
            client: reqwest::Client::builder()
                .build()
                .context("Failed to create HTTP client")?,
        })
    }

    fn build_request(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        stream: bool,
    ) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": options.model_id,
            "messages": messages,
            "stream": stream,
            "options": {
                "temperature": options.temperature,
                "num_predict": options.max_tokens,
            }
        });
        if !options.tools.is_empty() {
            body["tools"] =
                serde_json::to_value(&options.tools).unwrap_or_default();
        }
        body
    }
}

// ── Ollama chunk types ────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OllamaStreamChunk {
    #[allow(dead_code)]
    model: String,
    #[allow(dead_code)]
    created_at: String,
    message: Option<OllamaMessage>,
    done: bool,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OllamaMessage {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<String>,
    #[allow(dead_code)]
    tool_calls: Option<Vec<OllamaToolCall>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OllamaToolCall {
    #[allow(dead_code)]
    r#type: String,
    function: OllamaToolCallFunction,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OllamaToolCallFunction {
    name: String,
    arguments: serde_json::Value,
}

// Non-streaming response (populated by serde)
#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct OllamaChatResponse {
    message: OllamaMessage,
    #[allow(dead_code)]
    done: bool,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
}

// ── Trait impl ────────────────────────────────────────────

#[async_trait]
impl LlmProvider for OllamaProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResult> {
        let url = format!("{}/api/chat", self.base_url);
        let body = self.build_request(messages, options, false);
        tracing::debug!(
            "Ollama request prepared: messages={}, tools={}, stream=false",
            request_item_count(&body, "messages"),
            request_item_count(&body, "tools")
        );

        let response = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to Ollama")?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "Ollama API error ({}): {}",
                status,
                text
            ));
        }

        let completion: OllamaChatResponse = response.json().await?;
        tracing::debug!(
            "Ollama response received: has_content={}, tool_calls={}",
            completion.message.content.is_some(),
            completion.message.tool_calls.as_ref().map_or(0, Vec::len)
        );
        tracing::debug!(
            "← complete ({} in / {} out)",
            completion.prompt_eval_count.unwrap_or(0),
            completion.eval_count.unwrap_or(0)
        );
        let usage = Usage {
            prompt_tokens: completion.prompt_eval_count.unwrap_or(0),
            completion_tokens: completion.eval_count.unwrap_or(0),
            total_tokens: 0,
        };

        let tool_calls = completion
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: format!(
                    "ollama_{}_{}",
                    tc.function.name,
                    OLLAMA_CALL_ID.fetch_add(1, Ordering::Relaxed)
                ),
                call_type: "function".into(),
                function: ToolFunction {
                    name: tc.function.name,
                    arguments: serde_json::to_string(&tc.function.arguments)
                        .unwrap_or_default(),
                },
            })
            .collect();

        Ok(ChatResult {
            content: completion.message.content,
            tool_calls,
            usage,
            reasoning_content: None,
        })
    }

    async fn chat_stream(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
        sender: tokio::sync::mpsc::UnboundedSender<Result<StreamEvent>>,
    ) {
        let url = format!("{}/api/chat", self.base_url);
        let body = self.build_request(messages, options, true);
        tracing::debug!(
            "Ollama stream request prepared: messages={}, tools={}",
            request_item_count(&body, "messages"),
            request_item_count(&body, "tools")
        );

        let response = match self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = sender
                    .send(Err(anyhow::anyhow!("Ollama request failed: {}", e)));
                return;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let text = match response.text().await {
                Ok(t) => t,
                Err(_) => "unknown".into(),
            };
            let _ = sender.send(Err(anyhow::anyhow!(
                "Ollama API error ({}): {}",
                status,
                text
            )));
            return;
        }

        // NDJSON streaming with tool call support
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text_content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut usage = Usage::default();
        let mut saw_done = false;

        while let Some(chunk_result) = stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    let _ = sender
                        .send(Err(anyhow::anyhow!("Stream read error: {}", e)));
                    return;
                }
            };
            buf.extend_from_slice(&chunk);

            loop {
                match buf.iter().position(|&b| b == b'\n') {
                    Some(nl_pos) => {
                        let line_bytes: Vec<u8> = buf.drain(..nl_pos).collect();
                        buf.drain(..1);
                        let line = match std::str::from_utf8(&line_bytes) {
                            Ok(s) => s.trim(),
                            Err(_) => continue,
                        };
                        if line.is_empty() {
                            continue;
                        }

                        match serde_json::from_str::<OllamaStreamChunk>(line) {
                            Ok(chunk) => {
                                if let Some(msg) = chunk.message {
                                    if let Some(content) = msg.content {
                                        if !content.is_empty() {
                                            text_content.push_str(&content);
                                            let _ = sender.send(Ok(
                                                StreamEvent::Chunk(content),
                                            ));
                                        }
                                    }
                                    if let Some(tcs) = msg.tool_calls {
                                        for tc in tcs {
                                            tool_calls.push(ToolCall {
                                                id: format!(
                                                    "ollama_{}_{}",
                                                    tc.function.name,
                                                    OLLAMA_CALL_ID.fetch_add(
                                                        1,
                                                        Ordering::Relaxed
                                                    )
                                                ),
                                                call_type: "function".into(),
                                                function: ToolFunction {
                                                    name: tc.function.name,
                                                    arguments:
                                                        serde_json::to_string(
                                                            &tc.function
                                                                .arguments,
                                                        )
                                                        .unwrap_or_default(),
                                                },
                                            });
                                        }
                                    }
                                }
                                if chunk.done {
                                    saw_done = true;
                                    usage = Usage {
                                        prompt_tokens: chunk
                                            .prompt_eval_count
                                            .unwrap_or(0),
                                        completion_tokens: chunk
                                            .eval_count
                                            .unwrap_or(0),
                                        total_tokens: 0,
                                    };
                                }
                            }
                            Err(e) => tracing::warn!(
                                "Failed to parse Ollama chunk: {e} | line: {line}"
                            ),
                        }
                    }
                    None => break,
                }
            }
        }

        if !saw_done {
            let _ = sender.send(Err(anyhow::anyhow!(
                "Ollama stream ended before done=true"
            )));
            return;
        }

        let result = finalize_ollama(&mut text_content, &mut tool_calls, usage);
        tracing::debug!(
            "Ollama stream result finalized: content_len={}, tool_calls={}",
            result.content.as_ref().map_or(0, String::len),
            result.tool_calls.len()
        );
        let _ = sender.send(Ok(StreamEvent::Done(result)));
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        tracing::debug!("→ LIST models from Ollama");
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .context("Failed to fetch Ollama models")?;
        let data: serde_json::Value = resp.json().await?;
        let models: Vec<String> = data["models"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|m| m["name"].as_str().map(String::from))
            .collect();
        tracing::debug!("← {} models from Ollama", models.len());
        Ok(models)
    }
}

fn finalize_ollama(
    text: &mut String,
    tool_calls: &mut Vec<ToolCall>,
    usage: Usage,
) -> ChatResult {
    let calls = std::mem::take(tool_calls);
    // Keep text content even when tool calls exist — Ollama may stream text before tool calls
    let content = Some(std::mem::take(text));
    let content = if content.as_ref().map_or(true, |s| s.is_empty())
        && !calls.is_empty()
    {
        None
    } else {
        content
    };
    ChatResult {
        content,
        tool_calls: calls,
        usage,
        reasoning_content: None,
    }
}
