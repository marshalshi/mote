use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

pub struct AudioCapture {
    _stream: cpal::Stream,
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl AudioCapture {
    pub fn start(target_sample_rate: u32) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .context("No default microphone input device found")?;
        let device_name = device.name().unwrap_or_else(|_| "unknown".into());
        let supported = device
            .default_input_config()
            .context("Failed to read default microphone config")?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let source_sample_rate = config.sample_rate.0;
        let channels = config.channels.max(1) as usize;
        tracing::info!(
            device = %device_name,
            source_sample_rate,
            target_sample_rate,
            source_channels = config.channels,
            ?sample_format,
            "opening microphone input stream"
        );

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let err_fn = |err| tracing::warn!("microphone stream error: {err}");
        let stream = match sample_format {
            cpal::SampleFormat::I16 => device.build_input_stream(
                &config,
                {
                    let buffer = Arc::clone(&buffer);
                    move |data: &[i16], _| {
                        append_i16_pcm(
                            data.iter().step_by(channels).copied(),
                            source_sample_rate,
                            target_sample_rate,
                            &buffer,
                        );
                    }
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::U16 => device.build_input_stream(
                &config,
                {
                    let buffer = Arc::clone(&buffer);
                    move |data: &[u16], _| {
                        append_i16_pcm(
                            data.iter()
                                .step_by(channels)
                                .map(|s| (*s as i32 - i16::MAX as i32 - 1) as i16),
                            source_sample_rate,
                            target_sample_rate,
                            &buffer,
                        );
                    }
                },
                err_fn,
                None,
            ),
            cpal::SampleFormat::F32 => device.build_input_stream(
                &config,
                {
                    let buffer = Arc::clone(&buffer);
                    move |data: &[f32], _| {
                        append_i16_pcm(
                            data.iter()
                                .step_by(channels)
                                .map(|s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16),
                            source_sample_rate,
                            target_sample_rate,
                            &buffer,
                        );
                    }
                },
                err_fn,
                None,
            ),
            other => {
                anyhow::bail!("Unsupported microphone sample format: {other:?}")
            }
        }
        .context("Failed to open microphone input stream")?;
        stream
            .play()
            .context("Failed to start microphone input stream")?;
        Ok(Self {
            _stream: stream,
            buffer,
        })
    }

    pub fn recorded_pcm(&self) -> Vec<u8> {
        self.buffer
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default()
    }
}

fn append_i16_pcm<I>(
    samples: I,
    source_sample_rate: u32,
    target_sample_rate: u32,
    buffer: &Arc<Mutex<Vec<u8>>>,
) where
    I: IntoIterator<Item = i16>,
{
    let samples: Vec<i16> = samples.into_iter().collect();
    let samples = resample_nearest(&samples, source_sample_rate, target_sample_rate);
    let mut bytes = Vec::new();
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    if !bytes.is_empty()
        && let Ok(mut buffer) = buffer.lock()
    {
        buffer.extend_from_slice(&bytes);
    }
}

fn resample_nearest(samples: &[i16], source_sample_rate: u32, target_sample_rate: u32) -> Vec<i16> {
    if source_sample_rate == 0
        || target_sample_rate == 0
        || source_sample_rate == target_sample_rate
    {
        return samples.to_vec();
    }
    let out_len =
        (samples.len() as u64 * target_sample_rate as u64 / source_sample_rate as u64) as usize;
    (0..out_len)
        .filter_map(|idx| {
            let src_idx =
                (idx as u64 * source_sample_rate as u64 / target_sample_rate as u64) as usize;
            samples.get(src_idx).copied()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_keeps_same_rate() {
        let samples = vec![1, 2, 3];
        assert_eq!(resample_nearest(&samples, 24_000, 24_000), samples);
    }

    #[test]
    fn resample_downsamples() {
        assert_eq!(resample_nearest(&[1, 2, 3, 4], 48_000, 24_000), vec![1, 3]);
    }
}
