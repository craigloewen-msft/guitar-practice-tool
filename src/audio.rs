//! Audio output (cpal) and offline rendering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use rtrb::{Consumer, Producer, RingBuffer};

use crate::synth::{Command, Engine, Shared};

/// Frames kept rendered ahead of the sound card. The engine (neural amp models in
/// particular) runs on its own thread so a slow block never starves the device;
/// ~85 ms at 48 kHz absorbs scheduling hiccups and is unnoticeable for practice.
const AHEAD_FRAMES: usize = 4096;
const RING_FRAMES: usize = 32768;
/// Frames rendered per engine call on the render thread.
const CHUNK_FRAMES: usize = 256;

/// Device callbacks that found the ring short of samples (diagnostics).
static UNDERRUNS: AtomicUsize = AtomicUsize::new(0);

pub fn underruns() -> usize {
    UNDERRUNS.load(Ordering::Relaxed)
}

pub struct AudioOut {
    _stream: cpal::Stream,
    render: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
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
        let channels = config.channels as usize;
        let shared = Arc::new(Shared::default());
        let (tx, rx) = channel();
        let engine = Engine::new(config.sample_rate as f32, shared.clone(), Some(rx));
        let sample_rate = config.sample_rate;
        if trace_enabled() {
            eprintln!(
                "[trace] build={} host={:?} device={:?} rate={} channels={} format={format} buffer={:?}",
                if cfg!(debug_assertions) { "dev profile (use --release for best performance)" } else { "release" },
                host.id(),
                device.description().map(|d| d.name().to_string()).unwrap_or_default(),
                sample_rate,
                channels,
                supported_buffer(&device),
            );
        }

        let (producer, consumer) = RingBuffer::<f32>::new(RING_FRAMES * channels);
        // Largest buffer the device has asked for, so the render thread stays ahead of it.
        let device_frames = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let render = {
            let (device_frames, stop) = (device_frames.clone(), stop.clone());
            std::thread::Builder::new()
                .name("audio-render".into())
                .spawn(move || render_loop(engine, producer, channels, device_frames, stop))?
        };
        let waker = render.thread().clone();
        let feed = Feed { consumer, device_frames, waker };
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, config, feed),
            cpal::SampleFormat::I16 => build::<i16>(&device, config, feed),
            cpal::SampleFormat::I32 => build::<i32>(&device, config, feed),
            cpal::SampleFormat::U16 => build::<u16>(&device, config, feed),
            other => Err(anyhow!("unsupported sample format {other}")),
        };
        let mut out = AudioOut { _stream: stream?, render: Some(render), stop, tx, shared, sample_rate };
        // Give the render thread a moment to fill the ring before the device starts pulling.
        std::thread::sleep(Duration::from_millis(20));
        if let Err(e) = out._stream.play() {
            out.shutdown();
            return Err(e.into());
        }
        Ok(out)
    }

    pub fn send(&self, c: Command) {
        let _ = self.tx.send(c);
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.render.take() {
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn trace_enabled() -> bool {
    std::env::var_os("GPT_TRACE").is_some_and(|v| !v.is_empty() && v != "0")
}

fn supported_buffer(device: &cpal::Device) -> String {
    device.default_output_config().map(|c| format!("{:?}", c.buffer_size())).unwrap_or_default()
}

/// Per-second render-thread statistics (`GPT_TRACE=1`).
struct Trace {
    since: Instant,
    frames: usize,
    busy: Duration,
    worst: Duration,
    min_fill: usize,
    underruns: usize,
}

impl Trace {
    fn new() -> Self {
        Trace { since: Instant::now(), frames: 0, busy: Duration::ZERO, worst: Duration::ZERO, min_fill: usize::MAX, underruns: underruns() }
    }

    fn report(&mut self, engine: &Engine, sr: f32, channels: usize, device_frames: usize, fill: usize) {
        let wall = self.since.elapsed().as_secs_f64();
        let audio = self.frames as f64 / sr as f64;
        let budget = CHUNK_FRAMES as f64 / sr as f64 * 1000.0;
        let u = underruns();
        eprintln!(
            "[trace] rendered {:.2}s audio in {:.2}s wall | engine busy {:.0}% of real time | chunk avg {:.2}ms max {:.2}ms (budget {:.2}ms) | ring min {} frames | device {} frames | underruns +{} | {}",
            audio,
            wall,
            if audio > 0.0 { 100.0 * self.busy.as_secs_f64() / audio } else { 0.0 },
            if self.frames > 0 { self.busy.as_secs_f64() * 1000.0 / (self.frames / CHUNK_FRAMES) as f64 } else { 0.0 },
            self.worst.as_secs_f64() * 1000.0,
            budget,
            if self.min_fill == usize::MAX { fill } else { self.min_fill } / channels,
            device_frames,
            u - self.underruns,
            engine.trace_line(),
        );
        *self = Trace { underruns: u, ..Trace::new() };
    }
}

fn render_loop(mut engine: Engine, mut producer: Producer<f32>, channels: usize, device_frames: Arc<AtomicUsize>, stop: Arc<AtomicBool>) {
    let mut buf = vec![0.0f32; CHUNK_FRAMES * channels];
    let sr = engine.sample_rate();
    let mut trace = trace_enabled().then(Trace::new);
    while !stop.load(Ordering::Relaxed) {
        if let Some(t) = &mut trace {
            if t.since.elapsed() >= Duration::from_secs(1) {
                let fill = producer.buffer().capacity() - producer.slots();
                t.report(&engine, sr, channels, device_frames.load(Ordering::Relaxed), fill);
            }
        }
        let target = AHEAD_FRAMES.max(2 * device_frames.load(Ordering::Relaxed)).min(RING_FRAMES) * channels;
        let buffered = producer.buffer().capacity() - producer.slots();
        if buffered + buf.len() > target {
            std::thread::park_timeout(Duration::from_millis(2));
            continue;
        }
        let t0 = Instant::now();
        engine.process(&mut buf, channels);
        if let Some(t) = &mut trace {
            let dt = t0.elapsed();
            t.busy += dt;
            t.worst = t.worst.max(dt);
            t.frames += CHUNK_FRAMES;
            t.min_fill = t.min_fill.min(buffered);
        }
        if let Ok(chunk) = producer.write_chunk_uninit(buf.len()) {
            chunk.fill_from_iter(buf.iter().copied());
        }
    }
}

/// Audio-callback side of the ring buffer.
struct Feed {
    consumer: Consumer<f32>,
    device_frames: Arc<AtomicUsize>,
    waker: std::thread::Thread,
}

fn build<T>(device: &cpal::Device, config: cpal::StreamConfig, mut feed: Feed) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut warned = false;
    let stream = device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            feed.device_frames.fetch_max(data.len() / channels, Ordering::Relaxed);
            let n = feed.consumer.slots().min(data.len());
            if let Ok(chunk) = feed.consumer.read_chunk(n) {
                for (o, s) in data.iter_mut().zip(chunk) {
                    *o = T::from_sample(s);
                }
            }
            // Ran dry (render thread late): pad with silence rather than stalling the device.
            if n < data.len() {
                UNDERRUNS.fetch_add(1, Ordering::Relaxed);
            }
            for o in &mut data[n..] {
                *o = T::from_sample(0.0f32);
            }
            feed.waker.unpark();
        },
        move |e| {
            // Occasional xruns are recovered by the backend; report the first one only.
            if !warned {
                eprintln!("audio: {e}");
                warned = true;
            }
        },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays the song through the real sound card with every track and the amp model
    /// loaded and checks the device was never starved:
    /// `cargo test --release live_playback -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_playback_has_no_underruns() {
        let Some(path) = crate::ui::screenshots::test_song() else {
            return;
        };
        let song = crate::gp::load_file(&path).unwrap();
        let tones: Vec<_> = song.tracks.iter().map(|t| t.default_tone()).collect();
        let sched = Arc::new(crate::timeline::build_schedule(&song, &tones));
        let out = AudioOut::start().expect("audio output");
        out.send(Command::SetSchedule(sched.clone()));
        if let Some(p) = crate::synth::nam::find_model() {
            out.send(Command::Nam(Some(Arc::new(crate::synth::nam::NamModel::load(&p).unwrap()))));
        }
        let lib = crate::synth::sampler::Library::new();
        for id in crate::synth::sampler::BankId::ALL {
            if let Some(b) = lib.load_now(id) {
                out.send(Command::Bank(id, b));
            }
        }
        let start = sched.timeline.occurrence(59, 0).map(|m| m.start).unwrap_or(0);
        out.send(Command::MasterVolume(0.05));
        out.send(Command::Seek(start));
        std::thread::sleep(Duration::from_millis(300));
        let before = underruns();
        out.send(Command::Play);
        std::thread::sleep(Duration::from_secs(10));
        let n = underruns() - before;
        assert!(out.shared.pos() > start as f64, "transport did not advance");
        assert_eq!(n, 0, "{n} device callbacks were starved");
    }
}
