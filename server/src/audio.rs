use anyhow::{Context, Result};
use axum::extract::ws::{Message, WebSocket};
use base64::Engine;
use marshaling_protocol::{AudioClientEvent, AudioServerEvent};
use serde_json::Value;

/// Receive microphone PCM over the local websocket, then submit the completed
/// push-to-talk recording to OpenAI's file-oriented transcription API.
pub async fn handle_transcription_socket(
    mut socket: WebSocket,
    config: crate::config::AudioConfig,
    api_key: String,
) {
    tracing::debug!(
        provider = %config.provider,
        model = %config.model,
        sample_rate = config.sample_rate,
        channels = config.channels,
        "audio transcription websocket connected"
    );
    if let Err(e) = run_transcription_socket(&mut socket, config, api_key).await
    {
        tracing::warn!("audio transcription socket failed: {e:#}");
        let _ = send_server_event(
            &mut socket,
            &AudioServerEvent::Error {
                message: format!("{e:#}"),
            },
        )
        .await;
    }
}

async fn run_transcription_socket(
    socket: &mut WebSocket,
    config: crate::config::AudioConfig,
    api_key: String,
) -> Result<()> {
    let start = match socket.recv().await {
        Some(Ok(Message::Text(text))) => {
            serde_json::from_str::<AudioClientEvent>(&text)
                .context("Invalid audio start event")?
        }
        Some(Ok(Message::Close(_))) | None => return Ok(()),
        _ => anyhow::bail!("Expected audio start event"),
    };
    let AudioClientEvent::Start { config: start_cfg } = start else {
        anyhow::bail!("First audio event must be start");
    };
    tracing::debug!(
        client_sample_rate = start_cfg.sample_rate,
        client_channels = start_cfg.channels,
        "received audio start event"
    );
    if start_cfg.channels != config.channels {
        anyhow::bail!(
            "Audio channel mismatch: client sent {}, server expects {}",
            start_cfg.channels,
            config.channels
        );
    }
    if start_cfg.sample_rate != config.sample_rate {
        anyhow::bail!(
            "Audio sample-rate mismatch: client sent {}, server expects {}",
            start_cfg.sample_rate,
            config.sample_rate
        );
    }

    send_server_event(socket, &AudioServerEvent::Started).await?;

    let mut pcm = Vec::new();
    let mut audio_chunk_count: u64 = 0;
    loop {
        let Some(msg) = socket.recv().await else {
            return Ok(());
        };
        match msg {
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<AudioClientEvent>(&text) {
                    Ok(AudioClientEvent::AudioChunk { data }) => {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(data.as_bytes())
                            .context("Invalid base64 audio chunk")?;
                        audio_chunk_count += 1;
                        tracing::trace!(
                            chunks = audio_chunk_count,
                            bytes = decoded.len(),
                            "buffering audio chunk"
                        );
                        pcm.extend_from_slice(&decoded);
                    }
                    Ok(AudioClientEvent::Stop) => {
                        tracing::info!(
                            chunks = audio_chunk_count,
                            pcm_bytes = pcm.len(),
                            "submitting audio transcription"
                        );
                        if pcm.is_empty() {
                            send_server_event(socket, &AudioServerEvent::Error {
                                message: "No microphone audio was captured before stop. Check microphone permission/device and try again.".into(),
                            }).await?;
                            return Ok(());
                        }
                        let transcript =
                            transcribe_with_openai(&config, &api_key, pcm)
                                .await?;
                        send_server_event(
                            socket,
                            &AudioServerEvent::TranscriptFinal {
                                text: transcript,
                            },
                        )
                        .await?;
                        send_server_event(socket, &AudioServerEvent::Stopped)
                            .await?;
                        return Ok(());
                    }
                    Ok(AudioClientEvent::Cancel) => {
                        tracing::debug!(
                            "audio transcription cancelled by client"
                        );
                        send_server_event(socket, &AudioServerEvent::Stopped)
                            .await?;
                        return Ok(());
                    }
                    Ok(AudioClientEvent::Start { .. }) => {}
                    Err(e) => {
                        send_server_event(
                            socket,
                            &AudioServerEvent::Error {
                                message: format!("Invalid audio event: {e}"),
                            },
                        )
                        .await?;
                    }
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(e) => anyhow::bail!("Audio websocket error: {e}"),
        }
    }
}

async fn transcribe_with_openai(
    config: &crate::config::AudioConfig,
    api_key: &str,
    pcm: Vec<u8>,
) -> Result<String> {
    let wav = pcm16_wav(&pcm, config.sample_rate, config.channels)
        .context("Failed to encode microphone audio as WAV")?;
    let model = transcription_model(&config.model);
    if model != config.model {
        tracing::warn!(
            configured_model = %config.model,
            effective_model = %model,
            "configured audio model is realtime-only; using transcription API model"
        );
    }

    let file = reqwest::multipart::Part::bytes(wav)
        .file_name("mote-audio.wav")
        .mime_str("audio/wav")
        .context("Invalid audio MIME type")?;
    let form = reqwest::multipart::Form::new()
        .part("file", file)
        .text("model", model)
        .text("response_format", "json");
    let url = transcription_url(config);
    let response = reqwest::Client::new()
        .post(&url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .with_context(|| {
            format!("Failed to call OpenAI transcription API at {url}")
        })?;
    let status = response.status();
    let body = response
        .text()
        .await
        .context("Failed to read OpenAI transcription response")?;
    if !status.is_success() {
        anyhow::bail!(
            "OpenAI transcription API returned {status}: {}",
            extract_openai_error(&body)
        );
    }
    parse_transcription_text(&body)
}

fn transcription_url(config: &crate::config::AudioConfig) -> String {
    if config.realtime_url.starts_with("http") {
        config.realtime_url.clone()
    } else {
        "https://api.openai.com/v1/audio/transcriptions".into()
    }
}

fn transcription_model(configured: &str) -> String {
    match configured {
        "gpt-realtime-whisper" => "gpt-4o-mini-transcribe".into(),
        model => model.into(),
    }
}

fn parse_transcription_text(body: &str) -> Result<String> {
    let value: Value = serde_json::from_str(body).with_context(|| {
        format!("Invalid OpenAI transcription JSON: {body}")
    })?;
    value
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .context("OpenAI transcription response did not include text")
}

fn extract_openai_error(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| value.get("message").and_then(Value::as_str))
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.to_string())
}

fn pcm16_wav(pcm: &[u8], sample_rate: u32, channels: u16) -> Result<Vec<u8>> {
    if channels == 0 {
        anyhow::bail!("WAV channel count must be greater than zero");
    }
    let data_len = u32::try_from(pcm.len()).context("Audio is too large")?;
    let riff_len = data_len.checked_add(36).context("Audio is too large")?;
    let byte_rate = sample_rate
        .checked_mul(u32::from(channels))
        .and_then(|v| v.checked_mul(2))
        .context("Invalid WAV byte rate")?;
    let block_align =
        channels.checked_mul(2).context("Invalid WAV block align")?;

    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    Ok(wav)
}

async fn send_server_event(
    socket: &mut WebSocket,
    event: &AudioServerEvent,
) -> Result<()> {
    let json = serde_json::to_string(event)
        .context("Failed to serialize audio server event")?;
    socket
        .send(Message::Text(json.into()))
        .await
        .context("Failed to send audio server event")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pcm16_wav_header() {
        let wav = pcm16_wav(&[1, 2, 3, 4], 24_000, 1).unwrap();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 4);
        assert_eq!(&wav[44..], &[1, 2, 3, 4]);
    }

    #[test]
    fn maps_realtime_model_to_transcription_model() {
        assert_eq!(
            transcription_model("gpt-realtime-whisper"),
            "gpt-4o-mini-transcribe"
        );
        assert_eq!(
            transcription_model("gpt-4o-transcribe"),
            "gpt-4o-transcribe"
        );
    }

    #[test]
    fn parses_transcription_json_text() {
        assert_eq!(
            parse_transcription_text(r#"{"text":"hello"}"#).unwrap(),
            "hello"
        );
    }

    #[test]
    fn extracts_openai_error_message() {
        assert_eq!(
            extract_openai_error(r#"{"error":{"message":"bad model"}}"#),
            "bad model"
        );
    }
}
