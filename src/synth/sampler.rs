//! Sample-based instruments loaded from SoundFont (SF2) banks.
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
    GuitarDirect,
    /// Humbucker DI with real palm-muted takes (low/mid notes) + FreePats DI (high notes).
    GuitarHybridDi,
    BassFinger,
    BassPicked,
    Drums,
}

impl BankId {
    pub const ALL: [BankId; 5] = [
        BankId::GuitarDirect,
        BankId::GuitarHybridDi,
        BankId::BassFinger,
        BankId::BassPicked,
        BankId::Drums,
    ];
    pub const COUNT: usize = Self::ALL.len();

    pub fn index(self) -> usize {
        self as usize
    }

    /// The SF2 file name starts with this prefix (FreePats release naming).
    pub fn file_prefix(self) -> &'static str {
        match self {
            BankId::GuitarDirect | BankId::GuitarHybridDi => "EGuitarFSBS-direct",
            BankId::BassFinger => "FingerBassYR",
            BankId::BassPicked => "PickedBassYR",
            BankId::Drums => "MuldjordKit",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BankId::GuitarDirect => "electric guitar (DI)",
            BankId::GuitarHybridDi => "humbucker guitar (DI)",
            BankId::BassFinger => "finger bass",
            BankId::BassPicked => "picked bass",
            BankId::Drums => "drum kit",
        }
    }

    /// Sound bank used by a track tone (`None`: synthesized).
    pub fn for_tone(tone: ToneKind) -> Option<BankId> {
        Some(match tone {
            ToneKind::Clean | ToneKind::Acoustic => BankId::GuitarDirect,
            ToneKind::Nam => BankId::GuitarHybridDi,
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
            BankId::GuitarDirect | BankId::GuitarHybridDi => 1.0,
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
    /// Recorded palm-muted take.
    muted: bool,
}

/// Highest key served by the humbucker recordings for open notes in the hybrid DI bank;
/// above this the FreePats DI samples are used (less pitch shifting).
const HYBRID_SPLIT: u8 = 62;
const HB_NOTES: [(&str, u8, u8, u8); 6] =
    [("G#1", 32, 0, 38), ("D#2", 39, 39, 43), ("G#2", 44, 44, 48), ("C#3", 49, 49, 52), ("F3", 53, 53, 57), ("A#3", 58, 58, 127)];

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
    /// Has recorded palm-muted samples.
    pub has_muted: bool,
    /// Matching zones are round-robin alternatives (pick one) rather than layers.
    round_robin: bool,
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
                    muted: false,
                });
            }
        }
        if zones.is_empty() {
            return Err(anyhow!("{}: no playable regions", path.display()));
        }
        Ok(Bank::finish(id, data, zones, false))
    }

    fn finish(id: BankId, mut data: Vec<i16>, zones: Vec<Zone>, round_robin: bool) -> Bank {
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
        let has_muted = zones.iter().any(|z| z.muted);
        Bank { id, data, zones, by_key, nearest, norm: 0.9 / peak, has_muted, round_robin }
    }

    /// Humbucker DI recordings (`humbucker-di/<note>_<rr>_<vel>[_muted].wav`) for low and
    /// palm-muted notes, plus the FreePats DI SF2 for high open notes. Keeps the DI level
    /// realistic (not normalised) because it drives an amp model.
    pub fn load_hybrid(hb_dir: &Path, sf2: &Path) -> Result<Bank> {
        let fp = Bank::load(BankId::GuitarDirect, sf2)?;
        let mut data: Vec<i16> = Vec::new();
        let mut zones = Vec::new();
        // Humbucker samples are 24-bit with peaks around 0.2-0.45; store them as i16 scaled by 2.
        const HB_SCALE: f32 = 2.0;
        for &(note, root, lo, hi) in &HB_NOTES {
            for muted in [false, true] {
                for vel in 1..=2u8 {
                    for rr in 1..=4u8 {
                        let f = hb_dir.join(format!("{note}_{rr}_{vel}{}.wav", if muted { "_muted" } else { "" }));
                        let (samples, rate) = read_wav_mono(&f)?;
                        let max = (MAX_SECS * rate) as usize;
                        let len = samples.len().min(max);
                        data.extend(std::iter::repeat_n(0, PAD_BEFORE));
                        let start = data.len();
                        let fade = ((FADE_SECS * rate) as usize).min(len);
                        for (i, &x) in samples[..len].iter().enumerate() {
                            let g = if samples.len() > max && i >= len - fade {
                                let u = 1.0 - (i + 1 - (len - fade)) as f32 / fade as f32;
                                u * u
                            } else {
                                1.0
                            };
                            data.push((x * HB_SCALE * g * 32767.0).clamp(-32767.0, 32767.0) as i16);
                        }
                        data.extend(std::iter::repeat_n(0, PAD_AFTER));
                        let key_hi = if muted { hi } else { hi.min(HYBRID_SPLIT) };
                        if lo > key_hi {
                            continue;
                        }
                        let (vel_lo, vel_hi) = if vel == 1 { (0, 95) } else { (96, 127) };
                        // Hard-picked strings ring 5-30 cents sharp; tune each take.
                        let seg = if muted { (0.03, 0.15) } else { (0.2, 0.7) };
                        let cents = measure_cents(&samples, rate, root as f32, seg).unwrap_or(0.0);
                        zones.push(Zone {
                            key_lo: lo,
                            key_hi,
                            vel_lo,
                            vel_hi,
                            root: root as f32 + cents / 100.0,
                            start,
                            len,
                            rate,
                            gl: 1.0 / HB_SCALE,
                            gr: 1.0 / HB_SCALE,
                            muted,
                        });
                    }
                }
            }
        }
        // FreePats DI zones for the high open notes, level-matched to the humbucker at the split.
        let rms = |d: &[i16], z: &Zone| {
            let a = z.start + (0.03 * z.rate) as usize;
            let b = (z.start + (0.5 * z.rate) as usize).min(z.start + z.len);
            let s: f64 = d[a..b].iter().map(|&x| (x as f64 / 32768.0).powi(2)).sum();
            (s / (b - a).max(1) as f64).sqrt() as f32
        };
        let hb_ref: Vec<f32> = zones
            .iter()
            .filter(|z| !z.muted && z.key_hi == HYBRID_SPLIT && z.vel_lo >= 96)
            .map(|z| rms(&data, z) * z.gl)
            .collect();
        let mut fp_ids = [0u32; MAX_LAYERS];
        let n = fp.find(HYBRID_SPLIT as i32, 110, false, 0, &mut fp_ids);
        let fp_ref: f32 = fp_ids[..n].iter().map(|&zi| rms(&fp.data, &fp.zones[zi as usize]) * fp.zones[zi as usize].gl).sum();
        let match_gain = if fp_ref > 0.0 && !hb_ref.is_empty() {
            hb_ref.iter().sum::<f32>() / hb_ref.len() as f32 / fp_ref
        } else {
            1.0
        };
        let offset = data.len() + PAD_BEFORE;
        data.extend(std::iter::repeat_n(0, PAD_BEFORE));
        data.extend_from_slice(&fp.data);
        for z in fp.zones.iter().filter(|z| z.key_hi > HYBRID_SPLIT) {
            zones.push(Zone {
                key_lo: z.key_lo.max(HYBRID_SPLIT + 1),
                start: z.start + offset,
                gl: z.gl * match_gain,
                gr: z.gr * match_gain,
                ..*z
            });
        }
        let mut bank = Bank::finish(BankId::GuitarHybridDi, data, zones, true);
        bank.norm = 1.0;
        Ok(bank)
    }

    pub fn memory_bytes(&self) -> usize {
        self.data.len() * 2
    }

    /// Zones for a key/velocity (stereo pairs yield two zones).
    /// `muted` selects recorded palm-muted takes when the bank has them; `rr` picks one
    /// round-robin alternative in round-robin banks.
    fn find(&self, key: i32, vel: i32, muted: bool, rr: u32, out: &mut [u32; MAX_LAYERS]) -> usize {
        let mut all = [0u32; 16];
        let n = self.find_layers(key, vel, muted && self.has_muted, &mut all);
        if self.round_robin && n > 1 {
            out[0] = all[rr as usize % n];
            return 1;
        }
        let n = n.min(MAX_LAYERS);
        out[..n].copy_from_slice(&all[..n]);
        n
    }

    fn find_layers(&self, key: i32, vel: i32, muted: bool, out: &mut [u32]) -> usize {
        let k = self.nearest[key.clamp(0, 127) as usize] as usize;
        let all = &self.by_key[k];
        let mut list = [0u32; 32];
        let mut len = 0;
        for &zi in all.iter().filter(|&&zi| self.zones[zi as usize].muted == muted) {
            if len < list.len() {
                list[len] = zi;
                len += 1;
            }
        }
        if len == 0 {
            for &zi in all.iter().take(list.len()) {
                list[len] = zi;
                len += 1;
            }
        }
        let list = &list[..len];
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

/// Per-voice performance variation (used for double-tracked guitars).
#[derive(Clone, Copy)]
pub struct Take {
    /// Round-robin selector.
    pub rr: u32,
    pub gl: f32,
    pub gr: f32,
    /// Samples before the note sounds.
    pub delay: u32,
    /// Semitones.
    pub detune: f32,
}

impl Default for Take {
    fn default() -> Self {
        Take { rr: 0, gl: 1.0, gr: 1.0, delay: 0, detune: 0.0 }
    }
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
    /// Playing a recorded palm-muted / dead-note take.
    real_mute: bool,
    take: Take,
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
            real_mute: false,
            take: Take::default(),
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
        if self.real_mute && ev.dead {
            self.lp_a = one_pole(1200.0);
            self.sustain_mul = decay(0.04);
        } else if self.real_mute && ev.palm_mute {
            // The recording already has the muted attack and decay.
            self.lp_a = 0.0;
            self.sustain_mul = decay(0.5);
        } else if ev.dead {
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

    /// Start a note. `key` is the sample key (guitar: MIDI pitch, drums: kit key).
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &mut self,
        ev_idx: usize,
        ev: &NoteEvent,
        bank: &Bank,
        key: i32,
        sr: f32,
        rng: &mut Rng,
        age: u64,
        take: Take,
    ) -> bool {
        let jitter = (rng.next() * 6.0).round() as i32;
        let vel = ((ev.velocity * 127.0).round() as i32 + jitter).clamp(1, 127);
        let mut zs = [0u32; MAX_LAYERS];
        let want_mute = !ev.drum && (ev.palm_mute || ev.dead);
        let n = bank.find(key, vel, want_mute, take.rr, &mut zs);
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
        self.take = take;
        self.real_mute = want_mute && zs[..n].iter().all(|&z| bank.zones[z as usize].muted);
        self.pitch = if ev.drum { 0.0 } else { ev.pitch + ev.curve_at(0.0) + take.detune };
        self.vib_phase = 0.0;
        self.lp = [[0.0; 2]; 2];
        self.env = 1.0;
        // Velocity layers carry most of the dynamics; add a gentle level curve on top.
        let vel_gain = match bank.id {
            BankId::Drums => 1.0,
            BankId::BassFinger | BankId::BassPicked => ev.velocity.powf(0.9) * 1.2,
            // Real DI level into an amp model: velocity layers do the work.
            BankId::GuitarHybridDi => (ev.velocity * 1.25).powf(0.4),
            _ => (ev.velocity * 1.25).powf(0.7),
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
        if self.take.delay > 0 {
            self.take.delay -= 1;
            if self.state == State::Choked {
                self.state = State::Idle;
            }
            return (0.0, 0.0);
        }
        if !self.drum {
            let mut target = ev.pitch + ev.curve_at(rel_ticks.max(0.0)) + self.take.detune;
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
        (l * g * self.take.gl, r * g * self.take.gr)
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

    fn find_humbucker_dir(&self) -> Option<PathBuf> {
        self.dirs.iter().map(|d| d.join("humbucker-di")).find(|d| d.join("A#3_4_2_muted.wav").is_file())
    }

    pub fn has_humbucker(&self) -> bool {
        self.find_humbucker_dir().is_some()
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
        let r = if id == BankId::GuitarHybridDi {
            let hb = self.find_humbucker_dir().ok_or(None)?;
            Bank::load_hybrid(&hb, &sf2)
        } else {
            Bank::load(id, &sf2)
        };
        r.map_err(|e| Some(e.to_string()))
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

/// Tuning of a recorded note relative to `midi`, in cents, measured by autocorrelation
/// over the time window `seg` (seconds). `None` if the estimate is implausible.
fn measure_cents(x: &[f32], rate: f32, midi: f32, seg: (f32, f32)) -> Option<f32> {
    let a = (seg.0 * rate) as usize;
    let b = ((seg.1 * rate) as usize).min(x.len());
    let x = x.get(a..b)?;
    let lag0 = rate / midi_to_hz(midi);
    let (lo, hi) = ((lag0 * 0.95) as usize, (lag0 * 1.05) as usize + 1);
    if x.len() < hi * 2 {
        return None;
    }
    let ac: Vec<f64> = (lo..=hi).map(|l| x[..x.len() - l].iter().zip(&x[l..]).map(|(&p, &q)| (p * q) as f64).sum()).collect();
    let i = (1..ac.len() - 1).max_by(|&i, &j| ac[i].total_cmp(&ac[j]))?;
    let (p, q, r) = (ac[i - 1], ac[i], ac[i + 1]);
    let den = p - 2.0 * q + r;
    let frac = if den.abs() > 1e-12 { 0.5 * (p - r) / den } else { 0.0 };
    let lag = lo as f64 + i as f64 + frac;
    let cents = (1200.0 * (lag0 as f64 / lag).log2()) as f32;
    (cents.abs() < 60.0).then_some(cents)
}

/// Read a WAV file as mono f32 (first channel).
fn read_wav_mono(path: &Path) -> Result<(Vec<f32>, f32)> {
    let mut r = hound::WavReader::open(path).with_context(|| format!("open {}", path.display()))?;
    let spec = r.spec();
    let ch = spec.channels.max(1) as usize;
    let x: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => r.samples::<f32>().step_by(ch).collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            r.samples::<i32>().step_by(ch).map(|s| s.map(|v| v as f32 * scale)).collect::<Result<_, _>>()?
        }
    };
    Ok((x, spec.sample_rate as f32))
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
        assert!(v.start(0, ev, bank, ev.pitch as i32, sr, &mut rng, 0, Take::default()));
        (0..(secs * sr) as usize).map(|i| {
            let (l, r) = v.process(ev, i as f64 * 0.04, sr, bank);
            0.5 * (l + r)
        }).collect()
    }

    /// Runs only when the sample banks have been downloaded (`--download-sounds`).
    #[test]
    fn sampled_pitch_and_palm_mute() {
        let lib = Library::new();
        for id in [BankId::GuitarDirect, BankId::BassFinger] {
            let Some(bank) = lib.load_now(id) else {
                eprintln!("skipping: {} bank not installed", id.label());
                continue;
            };
            let pitches: &[f32] = if id == BankId::BassFinger { &[28.0, 33.0, 40.0, 50.0] } else { &[40.0, 47.0, 57.0, 64.0, 71.0] };
            for &p in pitches {
                let ev = NoteEvent { end: 100_000, pitch: p, velocity: 0.8, ..Default::default() };
                let y = render(&bank, &ev, 0.6);
                assert!(y.iter().all(|s| s.is_finite()));
                let hz = estimate_hz(&y[(0.15 * 48000.0) as usize..], 48000.0);
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

    /// Runs only when the humbucker DI + FreePats DI are installed.
    #[test]
    fn hybrid_di_pitch_and_recorded_palm_mutes() {
        let Some(bank) = Library::new().load_now(BankId::GuitarHybridDi) else {
            eprintln!("skipping: humbucker DI not installed");
            return;
        };
        assert!(bank.has_muted);
        // Both sides of the humbucker / FreePats split.
        for p in [40.0, 45.0, 52.0, 57.0, 62.0, 64.0, 71.0] {
            for rr in 0..2 {
                let ev = NoteEvent { end: 100_000, pitch: p, velocity: 0.8, ..Default::default() };
                let mut v = SampleVoice::new();
                let take = Take { rr, ..Take::default() };
                assert!(v.start(0, &ev, &bank, p as i32, 48000.0, &mut Rng::new(1), 0, take));
                let y: Vec<f32> = (0..28800).map(|i| v.process(&ev, i as f64 * 0.04, 48000.0, &bank).0).collect();
                let hz = estimate_hz(&y[7200..], 48000.0);
                let c = 1200.0 * (hz / midi_to_hz(p)).log2();
                assert!(c.abs() < 20.0, "midi {p} rr {rr}: {hz:.1} Hz ({c:+.1} cents)");
            }
        }
        let ev = NoteEvent { end: 100_000, pitch: 40.0, velocity: 0.8, palm_mute: true, ..Default::default() };
        let mut v = SampleVoice::new();
        assert!(v.start(0, &ev, &bank, 40, 48000.0, &mut Rng::new(1), 0, Take::default()));
        assert!(v.real_mute, "palm mute should use a recorded muted take");
        let open = render(&bank, &NoteEvent { palm_mute: false, ..ev.clone() }, 0.6);
        let pm = render(&bank, &ev, 0.6);
        let tail = |y: &[f32]| y[(0.4 * 48000.0) as usize..].iter().map(|s| s * s).sum::<f32>();
        assert!(tail(&pm) < tail(&open) * 0.2, "recorded palm mute should decay faster");
    }

    #[test]
    fn drum_map_covers_common_kit_pieces() {
        for k in [35, 36, 38, 40, 42, 44, 46, 49, 51, 57, 41, 43, 45, 47, 48, 50] {
            assert!(gm_to_kit(k, false).is_some(), "GM drum {k} unmapped");
        }
        assert_ne!(gm_to_kit(36, false), gm_to_kit(36, true));
    }
}
