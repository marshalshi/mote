use crate::llm::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashMap;

/// Provider for GitHub Models API (OpenAI-compatible).
#[derive(Clone)]
pub struct GitHubModelsProvider {
    token: String,
    base_url: String,
    client: reqwest::Client,
}

impl GitHubModelsProvider {
    pub fn new(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
    ) -> Result<Self> {
        Ok(Self {
            token: config.resolve_github_token(auth)?,
            base_url: config.github_base_url()?,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
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
            "temperature": options.temperature,
            "max_tokens": options.max_tokens,
        });
        if !options.tools.is_empty() {
            body["tools"] =
                serde_json::to_value(&options.tools).unwrap_or_default();
            body["tool_choice"] = serde_json::json!("auto");
        }
        body
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_event_separator_supports_lf_and_crlf() {
        assert_eq!(find_event_separator(b"data: one\n\nrest"), Some((9, 2)));
        assert_eq!(
            find_event_separator(b"data: one\r\n\r\nrest"),
            Some((9, 4))
        );
        assert_eq!(find_event_separator(b"data: one"), None);
    }
}

// ── Streaming chunk types (OpenAI-compatible) ───────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChunkChoice {
    delta: ChunkDelta,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChunkDelta {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<String>,
    #[allow(dead_code)]
    tool_calls: Option<Vec<ChunkToolCall>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChunkToolCall {
    index: usize,
    id: Option<String>,
    function: Option<ChunkToolCallFunction>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ChunkToolCallFunction {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StreamChunk {
    choices: Vec<ChunkChoice>,
    usage: Option<UsageBody>,
    #[allow(dead_code)]
    model: Option<String>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UsageBody {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

// ── Non-streaming response types ────────────────────────

#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompleteChoice {
    message: CompleteMessage,
    #[allow(dead_code)]
    finish_reason: String,
}

#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompleteMessage {
    content: Option<String>,
    tool_calls: Option<Vec<ResponseToolCall>>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ResponseToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: String,
    function: ResponseToolCallFunction,
}

#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ResponseToolCallFunction {
    name: String,
    arguments: String,
}

#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CompletionResponse {
    choices: Vec<CompleteChoice>,
    usage: Option<UsageBody>,
}

// ── Pending tool call accumulator ───────────────────────

struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

fn request_item_count(body: &serde_json::Value, key: &str) -> usize {
    body.get(key).and_then(|v| v.as_array()).map_or(0, Vec::len)
}

fn find_event_separator(buf: &[u8]) -> Option<(usize, usize)> {
    let lf = buf.windows(2).position(|w| w == b"\n\n");
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n");
    match (lf, crlf) {
        (Some(l), Some(c)) if c < l => Some((c, 4)),
        (Some(l), _) => Some((l, 2)),
        (None, Some(c)) => Some((c, 4)),
        (None, None) => None,
    }
}

// ── Trait impl ──────────────────────────────────────────

#[async_trait]
impl LlmProvider for GitHubModelsProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResult> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_request(messages, options, false);
        tracing::debug!(
            "GitHub Models request prepared: messages={}, tools={}, stream=false",
            request_item_count(&body, "messages"),
            request_item_count(&body, "tools")
        );

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to GitHub Models")?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            tracing::error!("← GitHub Models {}: {}", status, text);
            return Err(anyhow::anyhow!(
                "GitHub Models API error ({}): {}",
                status,
                text
            ));
        }

        let completion: CompletionResponse = response.json().await?;
        tracing::debug!(
            "GitHub Models response received: choices={}",
            completion.choices.len()
        );
        let choice = completion
            .choices
            .into_iter()
            .next()
            .context("Empty choices")?;
        let usage = completion
            .usage
            .map(|u| Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
            })
            .unwrap_or_default();
        let content = choice.message.content;
        let tool_calls = choice
            .message
            .tool_calls
            .unwrap_or_default()
            .into_iter()
            .map(|tc| ToolCall {
                id: tc.id,
                call_type: tc.call_type,
                function: ToolFunction {
                    name: tc.function.name,
                    arguments: tc.function.arguments,
                },
            })
            .collect();

        Ok(ChatResult {
            content,
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
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_request(messages, options, true);
        tracing::debug!(
            "GitHub Models stream request prepared: messages={}, tools={}",
            request_item_count(&body, "messages"),
            request_item_count(&body, "tools")
        );

        let response = match self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = sender.send(Err(anyhow::anyhow!(
                    "GitHub Models request failed: {}",
                    e
                )));
                return;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let _ = sender.send(Err(anyhow::anyhow!(
                "GitHub Models API error ({}): {}",
                status,
                text
            )));
            return;
        }

        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text_content = String::new();
        let mut tool_call_acc: HashMap<usize, PendingToolCall> = HashMap::new();
        let mut usage = Usage::default();

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

            while let Some((event_end, sep_len)) = find_event_separator(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..event_end).collect();
                buf.drain(..sep_len);
                let event_str = match std::str::from_utf8(&event_bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                for line in event_str.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            let result = finalize_gh(
                                &mut text_content,
                                &mut tool_call_acc,
                                usage,
                            );
                            let _ = sender.send(Ok(StreamEvent::Done(result)));
                            return;
                        }
                        if let Ok(chunk) =
                            serde_json::from_str::<StreamChunk>(data)
                        {
                            if let Some(choice) =
                                chunk.choices.into_iter().next()
                            {
                                if let Some(text) = choice.delta.content {
                                    text_content.push_str(&text);
                                    let _ = sender
                                        .send(Ok(StreamEvent::Chunk(text)));
                                }
                                if let Some(tcs) = choice.delta.tool_calls {
                                    for tc in tcs {
                                        let entry = tool_call_acc
                                            .entry(tc.index)
                                            .or_insert(PendingToolCall {
                                                id: None,
                                                name: None,
                                                arguments: String::new(),
                                            });
                                        if let Some(id) = tc.id {
                                            entry.id = Some(id);
                                        }
                                        if let Some(name) = tc
                                            .function
                                            .as_ref()
                                            .and_then(|f| f.name.clone())
                                        {
                                            entry.name = Some(name);
                                        }
                                        if let Some(args) = tc
                                            .function
                                            .as_ref()
                                            .and_then(|f| f.arguments.clone())
                                        {
                                            entry.arguments.push_str(&args);
                                        }
                                    }
                                }
                            }
                            if let Some(u) = chunk.usage {
                                usage = Usage {
                                    prompt_tokens: u.prompt_tokens,
                                    completion_tokens: u.completion_tokens,
                                    total_tokens: u.total_tokens,
                                };
                            }
                        }
                    }
                }
            }
        }

        let result = finalize_gh(&mut text_content, &mut tool_call_acc, usage);
        let _ = sender.send(Ok(StreamEvent::Done(result)));
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        // GitHub Models doesn't have a public /models endpoint for PAT-based auth.
        // Return a curated list of commonly available models.
        Ok(vec![
            "gpt-4o".into(),
            "gpt-4o-mini".into(),
            "o3-mini".into(),
            "o4-mini".into(),
            "anthropic-claude-sonnet-4-20250514".into(),
            "deepseek-r1".into(),
            "deepseek-v3".into(),
            "meta-llama-4-scout".into(),
            "mistral-large".into(),
        ])
    }
}

fn finalize_gh(
    text: &mut String,
    acc: &mut HashMap<usize, PendingToolCall>,
    usage: Usage,
) -> ChatResult {
    let mut tool_calls: Vec<(usize, ToolCall)> = acc
        .drain()
        .filter_map(|(idx, ptc)| {
            let id = ptc.id?;
            let name = ptc.name?;
            Some((
                idx,
                ToolCall {
                    id,
                    call_type: "function".into(),
                    function: ToolFunction {
                        name,
                        arguments: ptc.arguments,
                    },
                },
            ))
        })
        .collect();
    tool_calls.sort_by_key(|(idx, _)| *idx);

    let content = Some(std::mem::take(text));
    let content = if content.as_ref().map_or(true, |s| s.is_empty())
        && !tool_calls.is_empty()
    {
        None
    } else {
        content
    };
    ChatResult {
        content,
        tool_calls: tool_calls.into_iter().map(|(_, tc)| tc).collect(),
        usage,
        reasoning_content: None,
    }
}
