use crate::llm::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone)]
pub struct DeepSeekProvider {
    provider_name: &'static str,
    api_key: String,
    base_url: String,
    chat_path: &'static str,
    models_path: &'static str,
    max_tokens_field: MaxTokensField,
    temperature_decimals: Option<u32>,
    reasoning_split: bool,
    client: reqwest::Client,
}

#[derive(Clone, Copy)]
enum MaxTokensField {
    MaxTokens,
    MaxCompletionTokens,
}

impl DeepSeekProvider {
    pub fn new(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
    ) -> Result<Self> {
        Ok(Self {
            provider_name: "DeepSeek",
            api_key: config.resolve_deepseek_api_key(auth)?,
            base_url: config.deepseek_base_url()?,
            chat_path: "/chat/completions",
            models_path: "/models",
            max_tokens_field: MaxTokensField::MaxTokens,
            temperature_decimals: None,
            reasoning_split: false,
            client: reqwest::Client::builder()
                .build()
                .context("Failed to create HTTP client")?,
        })
    }

    fn new_api_key_provider(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
        provider: &'static str,
        display_name: &'static str,
        chat_path: &'static str,
        models_path: &'static str,
        max_tokens_field: MaxTokensField,
        temperature_decimals: Option<u32>,
        reasoning_split: bool,
    ) -> Result<Self> {
        Ok(Self {
            provider_name: display_name,
            api_key: config.resolve_provider_api_key(auth, provider)?,
            base_url: config.provider_base_url(provider)?,
            chat_path,
            models_path,
            max_tokens_field,
            temperature_decimals,
            reasoning_split,
            client: reqwest::Client::builder()
                .build()
                .context("Failed to create HTTP client")?,
        })
    }

    pub fn new_glm(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
    ) -> Result<Self> {
        Self::new_api_key_provider(
            config,
            auth,
            "glm",
            "GLM",
            "/paas/v4/chat/completions",
            "/paas/v4/models",
            MaxTokensField::MaxTokens,
            Some(2),
            false,
        )
    }

    pub fn new_kimi(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
    ) -> Result<Self> {
        Self::new_api_key_provider(
            config,
            auth,
            "kimi",
            "Kimi",
            "/v1/chat/completions",
            "/v1/models",
            MaxTokensField::MaxCompletionTokens,
            None,
            false,
        )
    }

    pub fn new_minimax(
        config: &crate::config::Config,
        auth: &crate::auth::Auth,
    ) -> Result<Self> {
        Self::new_api_key_provider(
            config,
            auth,
            "minimax",
            "MiniMax",
            "/v1/chat/completions",
            "/v1/models",
            MaxTokensField::MaxCompletionTokens,
            None,
            true,
        )
    }

    fn normalize_temperature(&self, temperature: f32) -> f64 {
        let temperature = temperature as f64;
        if let Some(decimals) = self.temperature_decimals {
            let factor = 10_f64.powi(decimals as i32);
            (temperature * factor).round() / factor
        } else {
            temperature
        }
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
            "temperature": self.normalize_temperature(options.temperature),
        });
        match self.max_tokens_field {
            MaxTokensField::MaxTokens => {
                body["max_tokens"] = serde_json::json!(options.max_tokens);
            }
            MaxTokensField::MaxCompletionTokens => {
                body["max_completion_tokens"] =
                    serde_json::json!(options.max_tokens);
            }
        }
        if !options.tools.is_empty() {
            body["tools"] =
                serde_json::to_value(&options.tools).unwrap_or_default();
            body["tool_choice"] = serde_json::json!("auto");
        }
        if self.reasoning_split {
            body["reasoning_split"] = serde_json::json!(true);
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
    usage: Option<UsageBody>,
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
    reasoning_details: Option<Vec<ReasoningDetail>>,
}

#[allow(dead_code)]
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct ReasoningDetail {
    text: Option<String>,
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

// ── Trait impl ────────────────────────────────────────────

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    async fn chat(
        &self,
        messages: &[ChatMessage],
        options: &ChatOptions,
    ) -> Result<ChatResult> {
        let url = format!("{}{}", self.base_url, self.chat_path);
        let body = self.build_request(messages, options, false);
        tracing::debug!(
            "LLM request prepared: messages={}, tools={}, stream=false",
            request_item_count(&body, "messages"),
            request_item_count(&body, "tools")
        );
        tracing::debug!("→ POST {}{}", self.base_url, self.chat_path);

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .with_context(|| {
                format!("Failed to send request to {}", self.provider_name)
            })?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            tracing::error!("← {} {}: {}", self.provider_name, status, text);
            return Err(anyhow::anyhow!(
                "{} API error ({}): {}",
                self.provider_name,
                status,
                text
            ));
        }

        let completion: CompletionResponse = response.json().await?;
        tracing::debug!(
            "LLM response received: choices={}",
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
        tracing::debug!(
            "← complete ({} in / {} out)",
            usage.prompt_tokens,
            usage.completion_tokens
        );
        let content = choice.message.content;
        let reasoning_content =
            choice.message.reasoning_content.or_else(|| {
                choice.message.reasoning_details.map(|details| {
                    details
                        .into_iter()
                        .filter_map(|detail| detail.text)
                        .collect::<Vec<_>>()
                        .join("")
                })
            });
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
        let url = format!("{}{}", self.base_url, self.chat_path);
        let body = self.build_request(messages, options, true);
        tracing::debug!(
            "LLM stream request prepared: messages={}, tools={}",
            request_item_count(&body, "messages"),
            request_item_count(&body, "tools")
        );

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
                    "{} request failed: {}",
                    self.provider_name,
                    e
                )));
                return;
            }
        };

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            let _ = sender.send(Err(anyhow::anyhow!(
                "{} API error ({}): {}",
                self.provider_name,
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
                                if let Some(u) = choice.usage {
                                    usage = Usage {
                                        prompt_tokens: u.prompt_tokens,
                                        completion_tokens: u.completion_tokens,
                                        total_tokens: u.total_tokens,
                                    };
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

        let _ = sender.send(Err(anyhow::anyhow!(
            "{} stream ended before completion marker",
            self.provider_name
        )));
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        tracing::debug!("→ LIST models from {}", self.provider_name);
        let url = format!("{}{}", self.base_url, self.models_path);
        let resp = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send()
            .await
            .with_context(|| {
                format!("Failed to fetch {} models", self.provider_name)
            })?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::error!("← {} {}: {}", self.provider_name, status, body);
            anyhow::bail!(
                "{} model list API error ({}): {}",
                self.provider_name,
                status,
                body
            );
        }

        let data: Value = serde_json::from_str(&body).with_context(|| {
            format!(
                "Failed to parse {} model list response as JSON",
                self.provider_name
            )
        })?;
        let models = extract_model_ids(&data);
        if models.is_empty() {
            tracing::warn!(
                "{} model list response contained no recognizable model ids: {}",
                self.provider_name,
                safe_truncate_json(&data, 400)
            );
        }
        tracing::debug!(
            "← {} models from {}",
            models.len(),
            self.provider_name
        );
        Ok(models)
    }
}

fn extract_model_ids(value: &Value) -> Vec<String> {
    let candidates = [
        value.get("data"),
        value.get("models"),
        value.get("list"),
        value.get("result"),
        value.get("items"),
        value.get("data").and_then(|v| v.get("models")),
        value.get("data").and_then(|v| v.get("list")),
        value.get("result").and_then(|v| v.get("models")),
        Some(value),
    ];

    for candidate in candidates.into_iter().flatten() {
        let models = extract_model_ids_recursive(candidate);
        if !models.is_empty() {
            return models;
        }
    }
    Vec::new()
}

fn extract_model_ids_recursive(value: &Value) -> Vec<String> {
    match value {
        Value::Array(arr) => arr
            .iter()
            .flat_map(extract_model_ids_recursive)
            .collect::<Vec<_>>(),
        Value::Object(map) => {
            let mut models = Vec::new();
            for (key, val) in map {
                if is_model_id_key(key) {
                    if let Some(model) = val.as_str() {
                        models.push(model.to_string());
                    }
                }
                models.extend(extract_model_ids_recursive(val));
            }
            dedupe_strings(models)
        }
        Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null => {
            Vec::new()
        }
    }
}

fn is_model_id_key(key: &str) -> bool {
    matches!(
        key,
        "id" | "model"
            | "name"
            | "model_id"
            | "model_name"
            | "api_model"
            | "model_api"
    )
}

fn dedupe_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

fn safe_truncate_json(value: &Value, max_chars: usize) -> String {
    let text = value.to_string();
    if text.chars().count() <= max_chars {
        return text;
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{}...", truncated)
}

#[test]
fn test_glm_provider_uses_live_models_path() {
    let provider = DeepSeekProvider {
        provider_name: "GLM",
        api_key: "key".into(),
        base_url: "https://api.z.ai/api".into(),
        chat_path: "/paas/v4/chat/completions",
        models_path: "/paas/v4/models",
        max_tokens_field: MaxTokensField::MaxTokens,
        temperature_decimals: Some(2),
        reasoning_split: false,
        client: reqwest::Client::new(),
    };

    assert_eq!(provider.models_path, "/paas/v4/models");
}

#[test]
fn test_extract_model_ids_handles_openai_shape() {
    let value = serde_json::json!({
        "data": [
            {"id": "kimi-k2.6"},
            {"id": "kimi-k2.7-code"}
        ]
    });

    assert_eq!(
        extract_model_ids(&value),
        vec!["kimi-k2.6".to_string(), "kimi-k2.7-code".to_string()]
    );
}

#[test]
fn test_extract_model_ids_handles_nested_models_shape() {
    let value = serde_json::json!({
        "data": {
            "models": [
                {"name": "glm-5.2"},
                {"name": "glm-4.7-flash"}
            ]
        }
    });

    assert_eq!(
        extract_model_ids(&value),
        vec!["glm-5.2".to_string(), "glm-4.7-flash".to_string()]
    );
}

#[test]
fn test_extract_model_ids_handles_model_id_variants() {
    let value = serde_json::json!({
        "result": {
            "items": [
                {"model_id": "glm-5.2"},
                {"model_name": "glm-4.7-flash"},
                {"api_model": "glm-4.6"}
            ]
        }
    });

    assert_eq!(
        extract_model_ids(&value),
        vec![
            "glm-5.2".to_string(),
            "glm-4.7-flash".to_string(),
            "glm-4.6".to_string()
        ]
    );
}

#[test]
fn test_extract_model_ids_ignores_blank_ids() {
    let value = serde_json::json!({
        "data": [
            {"id": ""},
            {"id": "   "},
            {"name": "kimi-k2.6"},
            {"model_id": "\tkimi-k2.7\n"}
        ]
    });

    assert_eq!(
        extract_model_ids(&value),
        vec!["kimi-k2.6".to_string(), "kimi-k2.7".to_string()]
    );
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

    #[test]
    fn test_build_request_uses_max_completion_tokens_when_configured() {
        let provider = DeepSeekProvider {
            provider_name: "Kimi",
            api_key: "key".into(),
            base_url: "https://api.example.com".into(),
            chat_path: "/v1/chat/completions",
            models_path: "/v1/models",
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            temperature_decimals: None,
            reasoning_split: false,
            client: reqwest::Client::new(),
        };
        let options = ChatOptions {
            model_id: "kimi-k2.6".into(),
            max_tokens: 123,
            ..ChatOptions::default()
        };

        let body =
            provider.build_request(&[ChatMessage::user("hi")], &options, true);

        assert_eq!(body["max_completion_tokens"], serde_json::json!(123));
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn test_build_request_enables_reasoning_split_for_minimax() {
        let provider = DeepSeekProvider {
            provider_name: "MiniMax",
            api_key: "key".into(),
            base_url: "https://api.example.com".into(),
            chat_path: "/v1/chat/completions",
            models_path: "/v1/models",
            max_tokens_field: MaxTokensField::MaxCompletionTokens,
            temperature_decimals: None,
            reasoning_split: true,
            client: reqwest::Client::new(),
        };

        let body = provider.build_request(
            &[ChatMessage::user("hi")],
            &ChatOptions::default(),
            false,
        );

        assert_eq!(body["reasoning_split"], serde_json::json!(true));
    }

    #[test]
    fn test_build_request_rounds_temperature_for_glm() {
        let provider = DeepSeekProvider {
            provider_name: "GLM",
            api_key: "key".into(),
            base_url: "https://api.example.com".into(),
            chat_path: "/paas/v4/chat/completions",
            models_path: "/paas/v4/models",
            max_tokens_field: MaxTokensField::MaxTokens,
            temperature_decimals: Some(2),
            reasoning_split: false,
            client: reqwest::Client::new(),
        };

        let options = ChatOptions {
            model_id: "glm-5.2".into(),
            temperature: 0.12345,
            ..ChatOptions::default()
        };

        let body =
            provider.build_request(&[ChatMessage::user("hi")], &options, false);

        assert_eq!(body["temperature"], serde_json::json!(0.12));
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
