//! Sample-based bass and drums loaded from SoundFont (SF2) banks. Guitars are not sampled:
//! they are synthesized strings played through the neural amp model.
//!
//! `rustysynth` is used only to parse the SF2 file. Playback is a custom voice so that the
//! existing note events (bends, slides, vibrato, legato, palm mutes, dead notes) map onto
//! real recordings instead of a MIDI-style synthesizer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};

use super::dsp::{flush, midi_to_hz, Rng};
use super::string::State;
use crate::model::ToneKind;
use crate::timeline::NoteEvent;

/// Longest sample kept in memory; notes ringing longer than this fade out.
const MAX_SECS: f32 = 6.0;
const FADE_SECS: f32 = 0.8;
const PAD_BEFORE: usize = 2;
const PAD_AFTER: usize = 4;
pub const MAX_LAYERS: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum BankId {
    BassFinger,
    BassPicked,
    Drums,
}

impl BankId {
    pub const ALL: [BankId; 3] = [BankId::BassFinger, BankId::BassPicked, BankId::Drums];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }

    /// The SF2 file name starts with this prefix (FreePats release naming).
    pub fn file_prefix(self) -> &'static str {
        match self {
            BankId::BassFinger => "FingerBassYR",
            BankId::BassPicked => "PickedBassYR",
            BankId::Drums => "MuldjordKit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BankId::BassFinger => "finger bass",
            BankId::BassPicked => "picked bass",
            BankId::Drums => "drum kit",
        }
    }

    /// Sound bank used by a track tone (`None`: synthesized, e.g. guitars into the amp model).
    pub fn for_tone(tone: ToneKind) -> Option<BankId> {
        Some(match tone {
            ToneKind::Clean | ToneKind::Nam => return None,
            ToneKind::Bass => BankId::BassPicked,
            ToneKind::FingerBass => BankId::BassFinger,
            ToneKind::Drums => BankId::Drums,
        })
    }

    /// Level trim so every bank sits at a similar loudness in the mix (measured).
    pub fn trim(self) -> f32 {
        match self {
            BankId::BassFinger | BankId::BassPicked => 0.75,
            BankId::Drums => 2.4,
        }
    }

    /// Whether the samples already contain the amplifier (no amp simulation needed).
    pub fn is_processed(self) -> bool {
        self == BankId::Drums
    }
}

struct Zone {
    key_lo: u8,
    key_hi: u8,
    vel_lo: u8,
    vel_hi: u8,
    /// Effective root pitch (root key minus tuning).
    root: f32,
    start: usize,
    len: usize,
    /// Sample rate of the recording.
    rate: f32,
    gl: f32,
    gr: f32,
}

pub struct Bank {
    pub id: BankId,
    data: Vec<i16>,
    zones: Vec<Zone>,
    /// Zone indices covering each key.
    by_key: Vec<Vec<u32>>,
    /// Nearest key that has zones (for notes outside the sampled range).
    nearest: Vec<u8>,
    /// Gain that brings the loudest sample in the bank to roughly full scale.
    pub norm: f32,
}

impl Bank {
    pub fn load(id: BankId, path: &Path) -> Result<Bank> {
        let mut f = std::io::BufReader::new(std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?);
        let sf = rustysynth::SoundFont::new(&mut f).map_err(|e| anyhow!("{}: {e:?}", path.display()))?;
        let wave = sf.get_wave_data();
        let headers = sf.get_sample_headers();
        let preset = sf.get_presets().first().ok_or_else(|| anyhow!("{}: no presets", path.display()))?;

        let mut data: Vec<i16> = Vec::new();
        let mut copied: HashMap<(i32, i32), (usize, usize)> = HashMap::new();
        let mut zones = Vec::new();
        for pr in preset.get_regions() {
            let Some(inst) = sf.get_instruments().get(pr.get_instrument_id()) else { continue };
            for r in inst.get_regions() {
                let key_lo = r.get_key_range_start().max(pr.get_key_range_start());
                let key_hi = r.get_key_range_end().min(pr.get_key_range_end());
                let vel_lo = r.get_velocity_range_start().max(pr.get_velocity_range_start());
                let vel_hi = r.get_velocity_range_end().min(pr.get_velocity_range_end());
                if key_lo > key_hi || vel_lo > vel_hi {
                    continue;
                }
                let Some(sh) = headers.get(r.get_sample_id()) else { continue };
                let rate = sh.get_sample_rate().max(1) as f32;
                let s0 = r.get_sample_start().max(0);
                let s1 = r.get_sample_end().min(wave.len() as i32);
                if s1 - s0 < 16 {
                    continue;
                }
                let (start, len) = *copied.entry((s0, s1)).or_insert_with(|| {
                    let src = &wave[s0 as usize..s1 as usize];
                    let max = (MAX_SECS * rate) as usize;
                    let len = src.len().min(max);
                    data.extend(std::iter::repeat(0).take(PAD_BEFORE));
                    let start = data.len();
                    data.extend_from_slice(&src[..len]);
                    if src.len() > max {
                        let fade = ((FADE_SECS * rate) as usize).min(len);
                        for (i, s) in data[start + len - fade..start + len].iter_mut().enumerate() {
                            let g = 1.0 - (i + 1) as f32 / fade as f32;
                            *s = (*s as f32 * g * g) as i16;
                        }
                    }
                    data.extend(std::iter::repeat(0).take(PAD_AFTER));
                    (start, len)
                });
                let tune = r.get_coarse_tune() as f32 + r.get_fine_tune() as f32 / 100.0;
                let atten_db = r.get_initial_attenuation() + pr.get_initial_attenuation();
                let gain = 10f32.powf(-atten_db / 20.0);
                let pan = ((r.get_pan() + pr.get_pan()) / 50.0).clamp(-1.0, 1.0);
                zones.push(Zone {
                    key_lo: key_lo.clamp(0, 127) as u8,
                    key_hi: key_hi.clamp(0, 127) as u8,
                    vel_lo: vel_lo.clamp(0, 127) as u8,
                    vel_hi: vel_hi.clamp(0, 127) as u8,
                    root: r.get_root_key() as f32 - tune,
                    start,
                    len,
                    rate,
                    gl: gain * (1.0 - pan).min(1.0),
                    gr: gain * (1.0 + pan).min(1.0),
                });
            }
        }
        if zones.is_empty() {
            return Err(anyhow!("{}: no playable regions", path.display()));
        }
        Ok(Bank::finish(id, data, zones))
    }

    fn finish(id: BankId, mut data: Vec<i16>, zones: Vec<Zone>) -> Bank {
        data.shrink_to_fit();
        let mut by_key: Vec<Vec<u32>> = vec![Vec::new(); 128];
        for (i, z) in zones.iter().enumerate() {
            for k in z.key_lo..=z.key_hi {
                by_key[k as usize].push(i as u32);
            }
        }
        let nearest = (0..128i32)
            .map(|k| {
                (0..128i32)
                    .filter(|&c| !by_key[c as usize].is_empty())
                    .min_by_key(|&c| (c - k).abs())
                    .unwrap_or(k) as u8
            })
            .collect();
        let peak = data.iter().map(|&s| (s as i32).unsigned_abs()).max().unwrap_or(1).max(1) as f32 / 32768.0;
        Bank { id, data, zones, by_key, nearest, norm: 0.9 / peak }
    }

    pub fn memory_bytes(&self) -> usize {
        self.data.len() * 2
    }

    /// Zones for a key/velocity (stereo pairs yield two zones).
    fn find(&self, key: i32, vel: i32, out: &mut [u32; MAX_LAYERS]) -> usize {
        let k = self.nearest[key.clamp(0, 127) as usize] as usize;
        let list = &self.by_key[k];
        let mut n = 0;
        for &zi in list {
            let z = &self.zones[zi as usize];
            if (z.vel_lo as i32..=z.vel_hi as i32).contains(&vel) && n < out.len() {
                out[n] = zi;
                n += 1;
            }
        }
        if n == 0 {
            // No exact velocity match: use the closest velocity layer.
            let dist = |zi: u32| {
                let z = &self.zones[zi as usize];
                if vel < z.vel_lo as i32 {
                    z.vel_lo as i32 - vel
                } else {
                    vel - z.vel_hi as i32
                }
            };
            if let Some(best) = list.iter().map(|&zi| dist(zi)).min() {
                for &zi in list {
                    if dist(zi) == best && n < out.len() {
                        out[n] = zi;
                        n += 1;
                    }
                }
            }
        }
        n
    }
}

/// Map a General MIDI drum key to the MuldjordKit layout (keys 48-66).
pub fn gm_to_kit(key: i32, alt_kick: bool) -> Option<i32> {
    Some(match key {
        35 | 36 => {
            if alt_kick {
                49
            } else {
                48
            }
        }
        37 => 65,
        38 | 39 | 40 => 50,
        42 | 44 | 54 | 69 | 70 => 52,
        46 => 53,
        49 => 58,
        55 | 57 => 59,
        52 => 60,
        51 => 54,
        59 => 56,
        53 => 55,
        56 => 57,
        50 => 61,
        48 => 62,
        47 | 45 => 63,
        43 | 41 => 64,
        _ => return None,
    })
}

#[derive(Clone, Copy, Default)]
struct Layer {
    zone: u32,
    pos: f64,
    done: bool,
}

pub struct SampleVoice {
    pub state: State,
    pub track: usize,
    pub string: usize,
    pub ev: usize,
    pub age: u64,
    pub bank: usize,
    pub drum: bool,
    /// Kit key for drum voices.
    pub key: i32,
    layers: [Layer; MAX_LAYERS],
    n_layers: usize,
    pitch: f32,
    gain: f32,
    env: f32,
    /// Per-sample envelope multiplier while sounding (palm mute / dead note decay).
    sustain_mul: f32,
    release_mul: f32,
    /// Two-pole lowpass (palm mute, dead notes, harmonics); `lp_a == 0` disables it.
    lp_a: f32,
    lp: [[f32; 2]; 2],
    vib_phase: f32,
}

impl SampleVoice {
    pub fn new() -> Self {
        SampleVoice {
            state: State::Idle,
            track: 0,
            string: 0,
            ev: 0,
            age: 0,
            bank: 0,
            drum: false,
            key: 0,
            layers: [Layer::default(); MAX_LAYERS],
            n_layers: 0,
            pitch: 60.0,
            gain: 1.0,
            env: 0.0,
            sustain_mul: 1.0,
            release_mul: 0.999,
            lp_a: 0.0,
            lp: [[0.0; 2]; 2],
            vib_phase: 0.0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state != State::Idle
    }

    pub fn level(&self) -> f32 {
        self.env * self.gain
    }

    fn articulation(&mut self, ev: &NoteEvent, bank: &Bank, sr: f32) {
        let f0 = midi_to_hz(ev.pitch);
        let one_pole = |fc: f32| (-std::f32::consts::TAU * fc.min(sr * 0.45) / sr).exp();
        let decay = |tau: f32| (-1.0 / (tau * sr)).exp();
        let processed = bank.id.is_processed();
        if ev.dead {
            self.lp_a = one_pole(if processed { 900.0 } else { 600.0 });
            self.sustain_mul = decay(0.03);
        } else if ev.palm_mute {
            let fc = if processed { 1300.0 + 2.0 * f0 } else { 450.0 + 3.0 * f0 };
            self.lp_a = one_pole(fc);
            self.sustain_mul = decay(if processed { 0.1 } else { 0.13 });
        } else if ev.harmonic {
            self.lp_a = one_pole(f0 * 2.5);
            self.sustain_mul = 1.0;
        } else {
            self.lp_a = 0.0;
            self.sustain_mul = 1.0;
        }
    }

    /// Start a note. `key` is the sample key (bass: MIDI pitch, drums: kit key).
    #[allow(clippy::too_many_arguments)]
    pub fn start(&mut self, ev_idx: usize, ev: &NoteEvent, bank: &Bank, key: i32, sr: f32, rng: &mut Rng, age: u64) -> bool {
        let jitter = (rng.next() * 6.0).round() as i32;
        let vel = ((ev.velocity * 127.0).round() as i32 + jitter).clamp(1, 127);
        let mut zs = [0u32; MAX_LAYERS];
        let n = bank.find(key, vel, &mut zs);
        if n == 0 {
            return false;
        }
        for (l, &z) in self.layers.iter_mut().zip(zs.iter()).take(n) {
            *l = Layer { zone: z, pos: 0.0, done: false };
        }
        self.n_layers = n;
        self.state = State::Sounding;
        self.track = ev.track;
        self.string = ev.string;
        self.ev = ev_idx;
        self.age = age;
        self.bank = bank.id.index();
        self.drum = ev.drum;
        self.key = key;
        self.pitch = if ev.drum { 0.0 } else { ev.pitch + ev.curve_at(0.0) };
        self.vib_phase = 0.0;
        self.lp = [[0.0; 2]; 2];
        self.env = 1.0;
        // Velocity layers carry most of the dynamics; add a gentle level curve on top.
        let vel_gain = match bank.id {
            BankId::Drums => 1.0,
            BankId::BassFinger | BankId::BassPicked => ev.velocity.powf(0.9) * 1.2,
        };
        self.gain = bank.norm * bank.id.trim() * vel_gain / (n as f32 / 2.0).max(1.0);
        if ev.drum {
            self.lp_a = 0.0;
            self.sustain_mul = 1.0;
        } else {
            self.articulation(ev, bank, sr);
        }
        self.release_mul = (-1.0 / (0.07 * sr)).exp();
        true
    }

    /// Continue the ringing sample at a new pitch (hammer-on / pull-off / legato slide).
    pub fn legato(&mut self, ev_idx: usize, ev: &NoteEvent, bank: &Bank, sr: f32) {
        self.ev = ev_idx;
        self.state = State::Sounding;
        self.vib_phase = 0.0;
        self.articulation(ev, bank, sr);
    }

    pub fn release(&mut self) {
        if self.state == State::Sounding {
            self.state = State::Released;
        }
    }

    /// Fade out quickly (used on pause for drums/cymbals too).
    pub fn release_with(&mut self, secs: f32, sr: f32) {
        if self.state != State::Idle && self.state != State::Choked {
            self.state = State::Released;
            self.release_mul = (-1.0 / (secs * sr)).exp();
        }
    }

    pub fn choke(&mut self) {
        if self.state != State::Idle {
            self.state = State::Choked;
        }
    }

    #[inline]
    pub fn process(&mut self, ev: &NoteEvent, rel_ticks: f64, sr: f32, bank: &Bank) -> (f32, f32) {
        if !self.drum {
            let mut target = ev.pitch + ev.curve_at(rel_ticks.max(0.0));
            if let Some((vstart, depth)) = ev.vibrato {
                if rel_ticks >= vstart as f64 {
                    self.vib_phase += 5.5 / sr;
                    if self.vib_phase > 1.0 {
                        self.vib_phase -= 1.0;
                    }
                    target += depth * 0.5 * (1.0 - (self.vib_phase * std::f32::consts::TAU).cos());
                }
            }
            self.pitch += (target - self.pitch) * 0.02;
        }

        let (mut l, mut r) = (0.0f32, 0.0f32);
        let mut alive = false;
        let mut cached: (f32, f64) = (f32::NAN, 1.0);
        for layer in self.layers.iter_mut().take(self.n_layers) {
            if layer.done {
                continue;
            }
            let z = &bank.zones[layer.zone as usize];
            let ratio = if self.drum {
                1.0
            } else {
                if cached.0 != z.root {
                    cached = (z.root, ((self.pitch - z.root) as f64 / 12.0).exp2());
                }
                cached.1
            };
            let step = ratio * z.rate as f64 / sr as f64;
            let i = layer.pos.floor();
            let frac = (layer.pos - i) as f32;
            let i = z.start + i as usize;
            let d = &bank.data;
            let xm1 = d[i - 1] as f32;
            let x0 = d[i] as f32;
            let x1 = d[i + 1] as f32;
            let x2 = d[i + 2] as f32;
            let c1 = 0.5 * (x1 - xm1);
            let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
            let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
            let y = (((c3 * frac + c2) * frac + c1) * frac + x0) * (1.0 / 32768.0);
            l += y * z.gl;
            r += y * z.gr;
            layer.pos += step;
            if layer.pos >= (z.len - 1) as f64 {
                layer.done = true;
            } else {
                alive = true;
            }
        }

        if self.lp_a > 0.0 {
            let a = self.lp_a;
            for (ch, x) in [&mut l, &mut r].into_iter().enumerate() {
                let s = &mut self.lp[ch];
                s[0] = flush(*x + a * (s[0] - *x));
                s[1] = flush(s[0] + a * (s[1] - s[0]));
                *x = s[1];
            }
        }

        match self.state {
            State::Sounding => self.env *= self.sustain_mul,
            State::Released => self.env *= self.release_mul * self.sustain_mul,
            State::Choked => self.env -= 1.0 / (0.006 * sr),
            State::Idle => {}
        }
        if !alive || self.env <= 1e-4 {
            self.state = State::Idle;
            self.env = 0.0;
        }
        let g = self.env.max(0.0) * self.gain;
        (l * g, r * g)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SlotState {
    Unrequested,
    Loading,
    Ready,
    Missing,
    Failed(String),
}

/// Finds SF2 files on disk and loads them in the background.
pub struct Library {
    dirs: Vec<PathBuf>,
    state: Mutex<Vec<SlotState>>,
}

impl Library {
    pub fn new() -> Arc<Library> {
        Arc::new(Library { dirs: search_dirs(), state: Mutex::new(vec![SlotState::Unrequested; BankId::COUNT]) })
    }

    pub fn dirs(&self) -> &[PathBuf] {
        &self.dirs
    }

    pub fn find_file(&self, id: BankId) -> Option<PathBuf> {
        self.dirs.iter().find_map(|d| find_sf2(d, id.file_prefix(), 3))
    }

    /// Forget banks that were missing so they are looked up again (after a download).
    pub fn rescan(&self) {
        for s in self.state.lock().unwrap().iter_mut() {
            if matches!(s, SlotState::Missing | SlotState::Failed(_)) {
                *s = SlotState::Unrequested;
            }
        }
    }

    /// `Err(None)`: files not installed.
    fn load_bank(&self, id: BankId) -> std::result::Result<Bank, Option<String>> {
        let sf2 = self.find_file(id).ok_or(None)?;
        Bank::load(id, &sf2).map_err(|e| Some(e.to_string()))
    }

    pub fn state(&self, id: BankId) -> SlotState {
        self.state.lock().unwrap()[id.index()].clone()
    }

    /// Load a bank once; `deliver` runs on the loader thread when it is ready.
    pub fn request(self: &Arc<Self>, id: BankId, deliver: impl FnOnce(Arc<Bank>) + Send + 'static) {
        {
            let mut st = self.state.lock().unwrap();
            if st[id.index()] != SlotState::Unrequested {
                return;
            }
            st[id.index()] = SlotState::Loading;
        }
        let lib = self.clone();
        std::thread::spawn(move || {
            let result = lib.load_bank(id);
            let new_state = match result {
                Ok(bank) => {
                    deliver(Arc::new(bank));
                    SlotState::Ready
                }
                Err(None) => SlotState::Missing,
                Err(Some(e)) => {
                    eprintln!("sound bank {}: {e}", id.label());
                    SlotState::Failed(e)
                }
            };
            lib.state.lock().unwrap()[id.index()] = new_state;
        });
    }

    /// Load synchronously (offline rendering).
    pub fn load_now(&self, id: BankId) -> Option<Arc<Bank>> {
        match self.load_bank(id) {
            Ok(b) => Some(Arc::new(b)),
            Err(None) => None,
            Err(Some(e)) => {
                eprintln!("sound bank {}: {e}", id.label());
                None
            }
        }
    }
}

pub(crate) fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("GPT_SOUNDS_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(PathBuf::from("assets"));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dirs.push(dir.join("assets"));
            // cargo layout: target/{debug,release}/<exe>
            if let Some(root) = dir.parent().and_then(|p| p.parent()) {
                dirs.push(root.join("assets"));
            }
        }
    }
    if let Some(d) = crate::setup::data_dir() {
        dirs.push(d);
    }
    dirs
}

fn find_sf2(dir: &Path, prefix: &str, depth: u32) -> Option<PathBuf> {
    let mut entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in &entries {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if p.is_file() && name.starts_with(prefix) && name.to_ascii_lowercase().ends_with(".sf2") {
            return Some(p.clone());
        }
    }
    if depth > 0 {
        for p in entries.iter().filter(|p| p.is_dir()) {
            if let Some(f) = find_sf2(p, prefix, depth - 1) {
                return Some(f);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::dsp::midi_to_hz;
    use crate::synth::string::tests::estimate_hz;

    fn render(bank: &Bank, ev: &NoteEvent, secs: f32) -> Vec<f32> {
        let sr = 48000.0;
        let mut v = SampleVoice::new();
        let mut rng = Rng::new(1);
        assert!(v.start(0, ev, bank, ev.pitch as i32, sr, &mut rng, 0));
        (0..(secs * sr) as usize).map(|i| {
            let (l, r) = v.process(ev, i as f64 * 0.04, sr, bank);
            0.5 * (l + r)
        }).collect()
    }

    /// Runs only when the sample banks have been downloaded (`--download-sounds`).
    #[test]
    fn sampled_pitch_and_palm_mute() {
        let lib = Library::new();
        for id in [BankId::BassFinger, BankId::BassPicked] {
            let Some(bank) = lib.load_now(id) else {
                eprintln!("skipping: {} bank not installed", id.label());
                continue;
            };
            let pitches: &[f32] = &[28.0, 33.0, 40.0, 50.0];
            for &p in pitches {
                let ev = NoteEvent { end: 100_000, pitch: p, velocity: 0.8, ..Default::default() };
                let y = render(&bank, &ev, 1.4);
                assert!(y.iter().all(|s| s.is_finite()));
                // Measure after the attack: hard-picked low strings start sharp and settle.
                let hz = estimate_hz(&y[(0.8 * 48000.0) as usize..], 48000.0);
                let c = 1200.0 * (hz / midi_to_hz(p)).log2();
                // allow octave-ambiguous estimates on bass, but the pitch class must be right
                let c = ((c + 600.0).rem_euclid(1200.0)) - 600.0;
                assert!(c.abs() < 15.0, "{} midi {p}: {hz:.1} Hz ({c:+.1} cents)", id.label());
            }
            // Palm mute: darker and quicker decay.
            let hf = |pm: bool| {
                let ev = NoteEvent { end: 100_000, pitch: 45.0, velocity: 0.8, palm_mute: pm, ..Default::default() };
                let y = render(&bank, &ev, 0.6);
                let d: f32 = y.windows(2).map(|w| (w[1] - w[0]).powi(2)).sum();
                let tail: f32 = y[(0.4 * 48000.0) as usize..].iter().map(|s| s * s).sum();
                (d, tail)
            };
            let (open_hf, open_tail) = hf(false);
            let (pm_hf, pm_tail) = hf(true);
            assert!(pm_hf < open_hf * 0.5, "{}: palm mute not darker", id.label());
            assert!(pm_tail < open_tail * 0.2, "{}: palm mute not shorter", id.label());
        }
    }

    #[test]
    fn drum_map_covers_common_kit_pieces() {
        for k in [35, 36, 38, 40, 42, 44, 46, 49, 51, 57, 41, 43, 45, 47, 48, 50] {
            assert!(gm_to_kit(k, false).is_some(), "GM drum {k} unmapped");
        }
        assert_ne!(gm_to_kit(36, false), gm_to_kit(36, true));
    }
}
