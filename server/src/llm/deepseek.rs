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
    /// Ask for token usage in the final stream chunk
    /// (`stream_options.include_usage`); only for APIs that document it.
    include_stream_usage: bool,
    timeouts: ProviderTimeouts,
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
            include_stream_usage: true,
            timeouts: ProviderTimeouts::from_config(config),
            client: provider_http_client()?,
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
            include_stream_usage: false,
            timeouts: ProviderTimeouts::from_config(config),
            client: provider_http_client()?,
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
        if stream && self.include_stream_usage {
            body["stream_options"] = serde_json::json!({"include_usage": true});
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
    for index in 0..buf.len() {
        let first_len = match buf[index] {
            b'\n' => 1,
            b'\r' if buf.get(index + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => continue,
        };
        let second = index + first_len;
        let second_len = match buf.get(second) {
            Some(b'\n' | b'\r') => {
                if buf.get(second) == Some(&b'\r')
                    && buf.get(second + 1) == Some(&b'\n')
                {
                    2
                } else {
                    1
                }
            }
            _ => continue,
        };
        return Some((index, first_len + second_len));
    }
    None
}

fn sse_event_data(event: &str) -> Option<String> {
    let normalized = event
        .strip_prefix('\u{feff}')
        .unwrap_or(event)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let data = normalized
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(|data| data.strip_prefix(' ').unwrap_or(data))
        .collect::<Vec<_>>()
        .join("\n");
    (!data.is_empty()).then_some(data)
}

fn finish_reason_signals_completion(reason: &str) -> bool {
    matches!(reason, "stop" | "length" | "content_filter" | "tool_calls")
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
            let headers = response.headers().clone();
            let text = response.text().await.unwrap_or_default();
            tracing::error!("← {} {}: {}", self.provider_name, status, text);
            return Err(ProviderError::from_response(
                self.provider_name,
                status,
                &headers,
                &text,
            )
            .into());
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
            finish_reason: Some(choice.finish_reason),
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

        let fail = |error: ProviderError| {
            let _ = sender.send(Err(error.into()));
        };
        let timeouts = self.timeouts;
        let request = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send();
        let response = match with_idle_timeout(
            timeouts.first_response,
            self.provider_name,
            request,
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                return fail(ProviderError::from_send_error(
                    self.provider_name,
                    &e,
                ));
            }
            Err(stalled) => return fail(stalled),
        };

        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let text = with_idle_timeout(
                timeouts.stream_idle,
                self.provider_name,
                response.text(),
            )
            .await
            .ok()
            .and_then(|text| text.ok())
            .unwrap_or_default();
            return fail(ProviderError::from_response(
                self.provider_name,
                status,
                &headers,
                &text,
            ));
        }

        // Parse streaming SSE events with tool call accumulation
        let mut stream = response.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text_content = String::new();
        let mut reasoning_content: Option<String> = None;
        let mut tool_call_acc: HashMap<usize, PendingToolCall> = HashMap::new();
        let mut usage = Usage::default();
        let mut saw_completion = false;
        let mut finish_reason: Option<String> = None;
        let mut received_data = false;

        loop {
            // The first data may be slow (queueing, long prompts); once it
            // flows, gaps between chunks are held to the idle limit.
            let limit = if received_data {
                timeouts.stream_idle
            } else {
                timeouts.first_response
            };
            let chunk_result = match with_idle_timeout(
                limit,
                self.provider_name,
                stream.next(),
            )
            .await
            {
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

            while let Some((event_end, sep_len)) = find_event_separator(&buf) {
                let event_bytes: Vec<u8> = buf.drain(..event_end).collect();
                buf.drain(..sep_len);
                let event_str = match std::str::from_utf8(&event_bytes) {
                    Ok(s) => s,
                    Err(error) => {
                        return fail(ProviderError::fatal(format!(
                            "Invalid UTF-8 in SSE event: {error}"
                        )));
                    }
                };

                let Some(data) = sse_event_data(event_str) else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() {
                    continue;
                }
                if data == "[DONE]" {
                    let result = finalize(
                        &mut text_content,
                        &mut tool_call_acc,
                        usage,
                        finish_reason.take(),
                        &mut reasoning_content,
                    );
                    let _ = sender.send(Ok(StreamEvent::Done(result)));
                    return;
                }
                match serde_json::from_str::<StreamChunk>(data) {
                    Ok(chunk) => {
                        if let Some(choice) = chunk.choices.into_iter().next() {
                            if let Some(text) = choice.delta.content {
                                text_content.push_str(&text);
                                let _ =
                                    sender.send(Ok(StreamEvent::Chunk(text)));
                            }
                            if let Some(ref rc) = choice.delta.reasoning_content
                            {
                                reasoning_content
                                    .get_or_insert(String::new())
                                    .push_str(rc);
                                let _ = sender.send(Ok(
                                    StreamEvent::ReasoningChunk(rc.clone()),
                                ));
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
                            if let Some(ref reason) = choice.finish_reason {
                                finish_reason = Some(reason.clone());
                                saw_completion |=
                                    finish_reason_signals_completion(reason);
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
                    Err(error) => {
                        // Gateways report failures mid-stream as
                        // `data: {"error": ...}`; surface and classify those.
                        let payload_error = serde_json::from_str::<Value>(data)
                            .ok()
                            .and_then(|payload| {
                                ProviderError::from_stream_payload(
                                    self.provider_name,
                                    &payload,
                                )
                            });
                        return fail(payload_error.unwrap_or_else(|| {
                            ProviderError::fatal(format!(
                                "Failed to parse SSE data event: {error}"
                            ))
                        }));
                    }
                }
            }
        }

        if saw_completion {
            let result = finalize(
                &mut text_content,
                &mut tool_call_acc,
                usage,
                finish_reason.take(),
                &mut reasoning_content,
            );
            let _ = sender.send(Ok(StreamEvent::Done(result)));
            return;
        }

        fail(ProviderError::transient(format!(
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
        include_stream_usage: false,
        timeouts: ProviderTimeouts {
            first_response: None,
            stream_idle: None,
        },
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
        assert_eq!(find_event_separator(b"data: one\r\rrest"), Some((9, 2)));
        assert_eq!(find_event_separator(b"data: one\r\n\nrest"), Some((9, 3)));
    }

    #[test]
    fn test_sse_event_data_supports_no_space_and_multiple_data_lines() {
        assert_eq!(
            sse_event_data("event: message\ndata:{\ndata:}\n"),
            Some("{\n}".into())
        );
        assert_eq!(
            sse_event_data("event: message\rdata: [DONE]\r"),
            Some("[DONE]".into())
        );
        assert_eq!(sse_event_data("\u{feff}data: first"), Some("first".into()));
        assert_eq!(sse_event_data("event: ping\n"), None);
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
            include_stream_usage: false,
            timeouts: ProviderTimeouts {
                first_response: None,
                stream_idle: None,
            },
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
            include_stream_usage: false,
            timeouts: ProviderTimeouts {
                first_response: None,
                stream_idle: None,
            },
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
            include_stream_usage: false,
            timeouts: ProviderTimeouts {
                first_response: None,
                stream_idle: None,
            },
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

    fn test_provider(include_stream_usage: bool) -> DeepSeekProvider {
        DeepSeekProvider {
            provider_name: "DeepSeek",
            api_key: "key".into(),
            base_url: "https://api.example.com".into(),
            chat_path: "/chat/completions",
            models_path: "/models",
            max_tokens_field: MaxTokensField::MaxTokens,
            temperature_decimals: None,
            reasoning_split: false,
            include_stream_usage,
            timeouts: ProviderTimeouts {
                first_response: None,
                stream_idle: None,
            },
            client: reqwest::Client::new(),
        }
    }

    #[test]
    fn test_build_request_asks_for_stream_usage_only_when_enabled() {
        let msgs = [ChatMessage::user("hi")];
        let options = ChatOptions::default();
        let body = test_provider(true).build_request(&msgs, &options, true);
        assert_eq!(
            body["stream_options"],
            serde_json::json!({"include_usage": true})
        );
        let body = test_provider(true).build_request(&msgs, &options, false);
        assert!(body.get("stream_options").is_none());
        let body = test_provider(false).build_request(&msgs, &options, true);
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn test_finalize_keeps_tool_calls_missing_id_or_name() {
        let mut acc = HashMap::from([
            (
                0,
                PendingToolCall {
                    id: None,
                    name: Some("read".into()),
                    arguments: "{}".into(),
                },
            ),
            (
                1,
                PendingToolCall {
                    id: Some("call_b".into()),
                    name: None,
                    arguments: "{}".into(),
                },
            ),
        ]);
        let result = finalize(
            &mut String::new(),
            &mut acc,
            Usage::default(),
            Some("tool_calls".into()),
            &mut None,
        );
        assert_eq!(result.tool_calls.len(), 2);
        assert!(result.tool_calls[0].id.starts_with("call_mote_"));
        assert_eq!(result.tool_calls[0].function.name, "read");
        assert_eq!(result.tool_calls[1].id, "call_b");
        assert_eq!(result.tool_calls[1].function.name, MISSING_TOOL_NAME);
    }

    #[test]
    fn test_finish_reason_signals_completion_for_terminal_stream_chunks() {
        assert!(finish_reason_signals_completion("stop"));
        assert!(finish_reason_signals_completion("length"));
        assert!(finish_reason_signals_completion("content_filter"));
        assert!(finish_reason_signals_completion("tool_calls"));
        assert!(!finish_reason_signals_completion("unknown"));
    }
}

/// Source of ids for streamed tool calls that arrive without one.
static GENERATED_CALL_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Per-process prefix for generated ids, so ids from a previous server run
/// (still in a resumed session's history) cannot collide with new ones.
static GENERATED_CALL_ID_PREFIX: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| {
        format!("{:x}", chrono::Utc::now().timestamp_millis())
    });

/// Name given to a streamed tool call that arrived without a function name.
const MISSING_TOOL_NAME: &str = "missing_tool_name";

fn finalize(
    text: &mut String,
    acc: &mut HashMap<usize, PendingToolCall>,
    usage: Usage,
    finish_reason: Option<String>,
    reasoning: &mut Option<String>,
) -> ChatResult {
    // Never drop a streamed tool call: some OpenAI-compatible gateways omit
    // the id, and a silently dropped call leaves a "tool_calls" turn with
    // nothing to run, so the model repeats itself until max steps. A missing
    // id is generated; a missing name becomes a placeholder the loop reports
    // back to the model as an unknown tool.
    let mut tool_calls: Vec<(usize, ToolCall)> = acc
        .drain()
        .map(|(idx, ptc)| {
            let id = ptc.id.filter(|id| !id.is_empty()).unwrap_or_else(|| {
                format!(
                    "call_mote_{}_{}",
                    *GENERATED_CALL_ID_PREFIX,
                    GENERATED_CALL_ID
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                )
            });
            let name = ptc
                .name
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| MISSING_TOOL_NAME.to_string());
            (
                idx,
                ToolCall {
                    id,
                    call_type: "function".into(),
                    function: ToolFunction {
                        name,
                        arguments: ptc.arguments,
                    },
                },
            )
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
        finish_reason,
        reasoning_content,
    }
}
