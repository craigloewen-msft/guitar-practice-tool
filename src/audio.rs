//! Audio output (cpal) and offline rendering.

use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

use crate::synth::{Command, Engine, Shared};

pub struct AudioOut {
    _stream: cpal::Stream,
    pub tx: Sender<Command>,
    pub shared: Arc<Shared>,
    pub sample_rate: u32,
}

impl AudioOut {
    pub fn start() -> Result<AudioOut> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or_else(|| anyhow!("no audio output device"))?;
        let supported = device.default_output_config()?;
        let format = supported.sample_format();
        let mut config: cpal::StreamConfig = supported.into();
        config.buffer_size = cpal::BufferSize::Default;
        let shared = Arc::new(Shared::default());
        let (tx, rx) = channel();
        let engine = Engine::new(config.sample_rate as f32, shared.clone(), Some(rx));
        let sample_rate = config.sample_rate;
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, engine)?,
            cpal::SampleFormat::I16 => build::<i16>(&device, config, engine)?,
            cpal::SampleFormat::I32 => build::<i32>(&device, config, engine)?,
            cpal::SampleFormat::U16 => build::<u16>(&device, config, engine)?,
            other => return Err(anyhow!("unsupported sample format {other}")),
        };
        stream.play()?;
        Ok(AudioOut { _stream: stream, tx, shared, sample_rate })
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, mut engine: Engine) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut scratch: Vec<f32> = vec![0.0; 16384];
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            if scratch.len() < data.len() {
                scratch.resize(data.len(), 0.0);
            }
            let buf = &mut scratch[..data.len()];
            engine.process(buf, channels);
            for (o, &s) in data.iter_mut().zip(buf.iter()) {
                *o = T::from_sample(s);
            }
        },
        |e| eprintln!("audio error: {e}"),
        None,
    )?;
    Ok(stream)
}

/// Write 16-bit stereo PCM WAV.
pub fn write_wav(path: &std::path::Path, sr: u32, interleaved: &[f32]) -> Result<()> {
    use std::io::Write;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data_len = (interleaved.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_len).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&sr.to_le_bytes())?;
    f.write_all(&(sr * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_len.to_le_bytes())?;
    for &s in interleaved {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        f.write_all(&v.to_le_bytes())?;
    }
    Ok(())
}
