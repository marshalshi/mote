use crate::llm::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use std::collections::HashMap;

#[derive(Clone)]
pub struct DeepSeekProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

impl DeepSeekProvider {
    pub fn new(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
    ) -> Result<Self> {
        Ok(Self {
            api_key: config.resolve_deepseek_api_key(auth)?,
            base_url: config.deepseek_base_url()?,
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

// ── Streaming chunk types ─────────────────────────────────

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
    reasoning_content: Option<String>,
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
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UsageBody {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

// ── Non-streaming response types (populated by serde) ─────

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
    reasoning_content: Option<String>,
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

// ── Pending tool call accumulator (for streaming) ─────────

struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

// ── Trait impl ────────────────────────────────────────────

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResult> {
        let url = format!("{}/chat/completions", self.base_url);
        let body = self.build_request(messages, options, false);
        // Log full request
        if let Ok(json) = serde_json::to_string_pretty(&body) {
            tracing::debug!(
                "─── LLM REQUEST ───────────────────────\n{}\n────────────────────────────────────",
                json
            );
        }
        tracing::debug!("→ POST {}/chat/completions", self.base_url);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("Failed to send request to DeepSeek")?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            tracing::error!("← DeepSeek {}: {}", status, text);
            return Err(anyhow::anyhow!(
                "DeepSeek API error ({}): {}",
                status,
                text
            ));
        }

        let completion: CompletionResponse = response.json().await?;
        // Log full response
        if let Ok(json) = serde_json::to_string_pretty(&completion) {
            tracing::debug!(
                "─── LLM RESPONSE ──────────────────────\n{}\n────────────────────────────────────",
                json
            );
        }
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
        tracing::debug!(
            "← complete ({} in / {} out)",
            usage.prompt_tokens,
            usage.completion_tokens
        );
        let content = choice.message.content;
        let reasoning_content = choice.message.reasoning_content;
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
            reasoning_content,
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
        if let Ok(json) = serde_json::to_string_pretty(&body) {
            tracing::debug!(
                "─── LLM STREAM REQUEST ────────────────\n{}\n────────────────────────────────────",
                json
            );
        }

        let response = match self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = sender.send(Err(anyhow::anyhow!(
                    "DeepSeek request failed: {}",
                    e
                )));
                return;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let _ = sender.send(Err(anyhow::anyhow!(
                "DeepSeek API error ({}): {}",
                status,
                text
            )));
            return;
        }

        // Parse streaming SSE events with tool call accumulation
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text_content = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_call_acc: HashMap<usize, PendingToolCall> = HashMap::new();
        let mut usage = Usage::default();

        fn find_double_newline(buf: &[u8]) -> Option<usize> {
            buf.windows(2).position(|w| w == b"\n\n")
        }

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

            while let Some(event_end) = find_double_newline(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..event_end).collect();
                buf.drain(..2); // \n\n
                let event_str = match std::str::from_utf8(&event_bytes) {
                    Ok(s) => s,
                    Err(_) => continue,
                };

                for line in event_str.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        let data = data.trim();
                        if data == "[DONE]" {
                            let result = finalize(
                                &mut text_content,
                                &mut tool_call_acc,
                                usage,
                                &mut reasoning_content,
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
                                // Accumulate text content
                                if let Some(text) = choice.delta.content {
                                    text_content.push_str(&text);
                                    let _ = sender
                                        .send(Ok(StreamEvent::Chunk(text)));
                                }
                                // Accumulate reasoning content (DeepSeek r1/v4 flash thinking)
                                if let Some(ref rc) =
                                    choice.delta.reasoning_content
                                {
                                    reasoning_content
                                        .get_or_insert(String::new())
                                        .push_str(rc);
                                    let _ = sender.send(Ok(
                                        StreamEvent::ReasoningChunk(rc.clone()),
                                    ));
                                }
                                // Accumulate tool call deltas
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
                                // Handle finish_reason
                                if let Some(ref reason) = choice.finish_reason {
                                    if reason == "tool_calls"
                                        || reason == "stop"
                                    {
                                        // We'll finalize after the loop
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

        let result = finalize(
            &mut text_content,
            &mut tool_call_acc,
            usage,
            &mut reasoning_content,
        );
        if let Ok(json) = serde_json::to_string_pretty(&result) {
            tracing::debug!(
                "─── LLM STREAM RESULT ────────────────\n{}\n────────────────────────────────────",
                json
            );
        }
        let _ = sender.send(Ok(StreamEvent::Done(result)));
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        tracing::debug!("→ LIST models from DeepSeek");
        let url = format!("{}/models", self.base_url);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .context("Failed to fetch DeepSeek models")?;
        let data: serde_json::Value = resp.json().await?;
        let models: Vec<String> = data["data"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|m| m["id"].as_str().map(String::from))
            .collect();
        tracing::debug!("← {} models from DeepSeek", models.len());
        Ok(models)
    }
}

fn finalize(
    text: &mut String,
    acc: &mut HashMap<usize, PendingToolCall>,
    usage: Usage,
    reasoning: &mut Option<String>,
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

    // Keep text content even when tool calls exist — DeepSeek may stream text before tool calls
    let content = Some(std::mem::take(text));
    let content = if content.as_ref().map_or(true, |s| s.is_empty())
        && !tool_calls.is_empty()
    {
        None
    } else {
        content
    };
    let reasoning_content = std::mem::take(reasoning);
    ChatResult {
        content,
        tool_calls: tool_calls.into_iter().map(|(_, tc)| tc).collect(),
        usage,
        reasoning_content,
    }
}
