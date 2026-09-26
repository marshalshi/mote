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
    timeouts: ProviderTimeouts,
    client: reqwest::Client,
}

/// Serialize messages for Ollama's `/api/chat`, which expects
/// `tool_calls[].function.arguments` as a JSON object; the shared
/// `ChatMessage` stores the OpenAI-style JSON string. Sending the string back
/// in history makes Ollama reject every request after the first tool call.
fn ollama_messages(messages: &[ChatMessage]) -> Vec<serde_json::Value> {
    messages
        .iter()
        .map(|message| {
            let mut value = serde_json::to_value(message).unwrap_or_default();
            if let Some(calls) = value
                .get_mut("tool_calls")
                .and_then(serde_json::Value::as_array_mut)
            {
                for call in calls {
                    if let Some(arguments) =
                        call.pointer_mut("/function/arguments")
                    {
                        if let Some(text) = arguments.as_str() {
                            // Ollama requires an object; anything else
                            // (invalid JSON, null, arrays) becomes `{}`.
                            *arguments = serde_json::from_str(text)
                                .ok()
                                .filter(serde_json::Value::is_object)
                                .unwrap_or_else(|| serde_json::json!({}));
                        }
                    }
                }
            }
            value
        })
        .collect()
}

impl OllamaProvider {
    pub fn new(config: &Config, _auth: &crate::auth::Auth) -> Result<Self> {
        Ok(Self {
            base_url: config.ollama_base_url()?,
            timeouts: ProviderTimeouts::from_config(config),
            client: provider_http_client()?,
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
            "messages": ollama_messages(messages),
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
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::from_response(
                "Ollama", status, &headers, &text,
            )
            .into());
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

        let had_tool_calls = completion.message.tool_calls.is_some();
        let content = completion.message.content;
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
            content,
            tool_calls,
            usage,
            finish_reason: Some(if had_tool_calls {
                "tool_calls".into()
            } else {
                "stop".into()
            }),
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

        let fail = |error: ProviderError| {
            let _ = sender.send(Err(error.into()));
        };
        let timeouts = self.timeouts;
        let request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send();
        // Ollama loads the model and evaluates the prompt before replying,
        // so the first response gets the longer limit.
        let response =
            match with_idle_timeout(timeouts.first_response, "Ollama", request)
                .await
            {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    return fail(ProviderError::from_send_error("Ollama", &e));
                }
                Err(stalled) => return fail(stalled),
            };

        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let text = match with_idle_timeout(
                timeouts.stream_idle,
                "Ollama",
                response.text(),
            )
            .await
            {
                Ok(Ok(t)) => t,
                _ => "unknown".into(),
            };
            return fail(ProviderError::from_response(
                "Ollama", status, &headers, &text,
            ));
        }

        // NDJSON streaming with tool call support
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text_content = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut received_data = false;

        loop {
            let limit = if received_data {
                timeouts.stream_idle
            } else {
                timeouts.first_response
            };
            let chunk_result =
                match with_idle_timeout(limit, "Ollama", stream.next()).await {
                    Ok(Some(chunk_result)) => chunk_result,
                    Ok(None) => break,
                    Err(stalled) => return fail(stalled),
                };
            received_data = true;
            let chunk = match chunk_result {
                Ok(chunk) => chunk,
                Err(error) => {
                    return fail(ProviderError::transient(format!(
                        "Stream read error: {error}"
                    )));
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
                            Err(error) => {
                                return fail(ProviderError::fatal(format!(
                                    "Invalid UTF-8 in Ollama stream record: {error}"
                                )));
                            }
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
                                    let usage = Usage {
                                        prompt_tokens: chunk
                                            .prompt_eval_count
                                            .unwrap_or(0),
                                        completion_tokens: chunk
                                            .eval_count
                                            .unwrap_or(0),
                                        total_tokens: 0,
                                    };
                                    let result = finalize_ollama(
                                        &mut text_content,
                                        &mut tool_calls,
                                        usage,
                                    );
                                    let _ = sender
                                        .send(Ok(StreamEvent::Done(result)));
                                    return;
                                }
                            }
                            Err(error) => {
                                // Ollama reports runner failures in-stream
                                // as `{"error": "..."}`.
                                let payload_error =
                                    serde_json::from_str::<serde_json::Value>(
                                        line,
                                    )
                                    .ok()
                                    .and_then(|payload| {
                                        ProviderError::from_stream_payload(
                                            "Ollama", &payload,
                                        )
                                    });
                                return fail(payload_error.unwrap_or_else(|| {
                                    ProviderError::fatal(format!(
                                        "Failed to parse Ollama stream record: {error}"
                                    ))
                                }));
                            }
                        }
                    }
                    None => break,
                }
            }
        }

        fail(ProviderError::transient(
            "Ollama stream ended before done=true",
        ));
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
        finish_reason: Some(if calls.is_empty() {
            "stop".into()
        } else {
            "tool_calls".into()
        }),
        tool_calls: calls,
        usage,
        reasoning_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ollama_messages_send_tool_arguments_as_objects() {
        let history = [
            ChatMessage::user("read it"),
            ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "ollama_read_0".into(),
                call_type: "function".into(),
                function: ToolFunction {
                    name: "read".into(),
                    arguments: r#"{"file_path":"a.txt"}"#.into(),
                },
            }]),
            ChatMessage::tool_result("ollama_read_0", "contents"),
        ];
        let messages = ollama_messages(&history);
        assert_eq!(
            messages[1]["tool_calls"][0]["function"]["arguments"],
            serde_json::json!({"file_path": "a.txt"})
        );
        // Everything else is serialized unchanged.
        assert_eq!(messages[0], serde_json::to_value(&history[0]).unwrap());
        assert_eq!(messages[2], serde_json::to_value(&history[2]).unwrap());
    }

    #[test]
    fn test_ollama_messages_replace_non_object_arguments() {
        for raw in ["null", "[1,2]", "\"x\"", "not json", ""] {
            let history = [ChatMessage::assistant_tool_calls(vec![ToolCall {
                id: "c".into(),
                call_type: "function".into(),
                function: ToolFunction {
                    name: "read".into(),
                    arguments: raw.into(),
                },
            }])];
            let messages = ollama_messages(&history);
            assert_eq!(
                messages[0]["tool_calls"][0]["function"]["arguments"],
                serde_json::json!({}),
                "{raw}"
            );
        }
    }
}
