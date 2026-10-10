//! Real-time playback engine: schedules note events into string/drum voices and mixes tracks.

pub mod amp;
pub mod drums;
pub mod dsp;
pub mod nam;
pub mod reverb;
pub mod sampler;
pub mod string;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::model::{ToneKind, TPQ};
use crate::timeline::Schedule;
use amp::AmpChain;
use drums::DrumVoice;
use dsp::{soft_limit, Rng};
use reverb::Reverb;
use nam::NamModel;
use sampler::{gm_to_kit, Bank, BankId, SampleVoice, Take};
use string::{State, StringVoice};

const MAX_STRING_VOICES: usize = 64;
const MAX_DRUM_VOICES: usize = 32;
const MAX_SAMPLE_VOICES: usize = 128;
/// Frames rendered per block (amp models process blocks).
const BLOCK: usize = 256;

/// How guitars on the "Metal (NAM amp)" tone are produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NamOptions {
    /// Two performances panned hard left/right, each through its own amp.
    pub double_track: bool,
    /// Use the FreePats DI only (no recorded palm mutes) instead of the hybrid DI.
    pub freepats_di: bool,
}

impl Default for NamOptions {
    fn default() -> Self {
        NamOptions { double_track: true, freepats_di: false }
    }
}

pub enum Command {
    SetSchedule(Arc<Schedule>),
    Play,
    Pause,
    Seek(u64),
    Speed(f64),
    Mute(usize, bool),
    Solo(usize, bool),
    Volume(usize, f32),
    Tone(usize, ToneKind),
    Loop(Option<(u64, u64)>),
    Metronome(bool),
    CountIn(bool),
    MasterVolume(f32),
    /// A sound bank finished loading.
    Bank(BankId, Arc<Bank>),
    /// Neural Amp Modeler capture for the "Metal (NAM amp)" tone.
    Nam(Option<Arc<NamModel>>),
    NamOptions(NamOptions),
}

/// State shared with the UI thread.
#[derive(Default)]
pub struct Shared {
    pos_bits: AtomicU64,
    pub playing: AtomicBool,
}

impl Shared {
    pub fn pos(&self) -> f64 {
        f64::from_bits(self.pos_bits.load(Ordering::Relaxed))
    }
    fn set_pos(&self, p: f64) {
        self.pos_bits.store(p.to_bits(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
struct Mix {
    mute: bool,
    solo: bool,
    volume: f32,
}

pub struct Engine {
    sr: f32,
    sched: Option<Arc<Schedule>>,
    pos: f64,
    playing: bool,
    speed: f64,
    next_ev: usize,
    next_click: usize,
    tempo_idx: usize,
    voices: Vec<StringVoice>,
    drums: Vec<DrumVoice>,
    svoices: Vec<SampleVoice>,
    banks: Vec<Option<Arc<Bank>>>,
    alt_kick: bool,
    chains: Vec<AmpChain>,
    tones: Vec<ToneKind>,
    mix: Vec<Mix>,
    /// Per-track dry L/R block buffers.
    track_buf: Vec<[Vec<f32>; 2]>,
    click_buf: Vec<f32>,
    nam: Option<Arc<NamModel>>,
    nam_opts: NamOptions,
    reverb: Reverb,
    rng: Rng,
    age: u64,
    loop_range: Option<(u64, u64)>,
    metronome: bool,
    count_in: bool,
    /// Remaining count-in clicks (ticks until playback starts).
    count_in_left: f64,
    click_t: f32,
    click_freq: f32,
    click_amp: f32,
    master: f32,
    shared: Arc<Shared>,
    rx: Option<Receiver<Command>>,
}

impl Engine {
    pub fn new(sr: f32, shared: Arc<Shared>, rx: Option<Receiver<Command>>) -> Self {
        Engine {
            sr,
            sched: None,
            pos: 0.0,
            playing: false,
            speed: 1.0,
            next_ev: 0,
            next_click: 0,
            tempo_idx: 0,
            voices: (0..MAX_STRING_VOICES).map(|_| StringVoice::new()).collect(),
            drums: (0..MAX_DRUM_VOICES).map(|_| DrumVoice::new()).collect(),
            svoices: (0..MAX_SAMPLE_VOICES).map(|_| SampleVoice::new()).collect(),
            banks: vec![None; BankId::COUNT],
            alt_kick: false,
            chains: Vec::new(),
            tones: Vec::new(),
            mix: Vec::new(),
            track_buf: Vec::new(),
            click_buf: vec![0.0; BLOCK],
            nam: None,
            nam_opts: NamOptions::default(),
            reverb: Reverb::new(sr),
            rng: Rng::new(0x1234_5678),
            age: 0,
            loop_range: None,
            metronome: false,
            count_in: false,
            count_in_left: 0.0,
            click_t: 1.0,
            click_freq: 1000.0,
            click_amp: 0.0,
            master: 0.8,
            shared,
            rx,
        }
    }

    pub fn handle(&mut self, cmd: Command) {
        match cmd {
            Command::SetSchedule(s) => {
                let n = s.tracks.len();
                // Keep mixer settings when the same song is rescheduled.
                if self.mix.len() != n {
                    self.mix = s.tracks.iter().map(|t| Mix { mute: false, solo: false, volume: t.volume }).collect();
                }
                self.tones = s.tracks.iter().map(|t| t.tone).collect();
                self.chains = (0..n).map(|i| self.make_chain(i)).collect();
                self.track_buf = (0..n).map(|_| [vec![0.0; BLOCK], vec![0.0; BLOCK]]).collect();
                self.sched = Some(s);
                let p = self.pos as u64;
                self.seek(p);
            }
            Command::Play => {
                if let Some(s) = &self.sched {
                    if self.pos as u64 >= s.timeline.total {
                        self.seek(0);
                    }
                }
                if !self.playing {
                    self.playing = true;
                    if self.count_in {
                        self.count_in_left = TPQ as f64 * 4.0;
                    }
                }
            }
            Command::Pause => {
                self.playing = false;
                self.count_in_left = 0.0;
                for v in &mut self.voices {
                    v.release();
                }
                for v in &mut self.svoices {
                    v.release_with(if v.drum { 0.15 } else { 0.07 }, self.sr);
                }
            }
            Command::Seek(t) => self.seek(t),
            Command::Speed(s) => self.speed = s.clamp(0.1, 2.0),
            Command::Mute(i, m) => {
                if let Some(x) = self.mix.get_mut(i) {
                    x.mute = m;
                }
            }
            Command::Solo(i, m) => {
                if let Some(x) = self.mix.get_mut(i) {
                    x.solo = m;
                }
            }
            Command::Volume(i, v) => {
                if let Some(x) = self.mix.get_mut(i) {
                    x.volume = v;
                }
            }
            Command::Tone(i, t) => {
                if i < self.tones.len() {
                    self.tones[i] = t;
                    self.chains[i] = self.make_chain(i);
                }
            }
            Command::Bank(id, bank) => {
                self.banks[id.index()] = Some(bank);
                for i in 0..self.tones.len() {
                    if self.track_bank(i).is_some_and(|b| b.id == id) {
                        self.chains[i] = self.make_chain(i);
                    }
                }
            }
            Command::Nam(m) => {
                self.nam = m;
                self.rebuild_nam_chains();
            }
            Command::NamOptions(o) => {
                self.nam_opts = o;
                self.rebuild_nam_chains();
            }
            Command::Loop(r) => self.loop_range = r.filter(|(a, b)| b > a),
            Command::Metronome(m) => self.metronome = m,
            Command::CountIn(c) => self.count_in = c,
            Command::MasterVolume(v) => self.master = v,
        }
    }

    fn rebuild_nam_chains(&mut self) {
        for i in 0..self.tones.len() {
            if self.tones[i] == ToneKind::Nam {
                for v in self.svoices.iter_mut().filter(|v| v.track == i) {
                    v.choke();
                }
                self.chains[i] = self.make_chain(i);
            }
        }
    }

    /// Loaded sound bank for a track's tone, if any.
    fn track_bank(&self, track: usize) -> Option<&Arc<Bank>> {
        let tone = *self.tones.get(track)?;
        if tone == ToneKind::Nam {
            // Prefer the DI with recorded palm mutes; the plain FreePats DI also works.
            let hybrid = self.banks[BankId::GuitarHybridDi.index()].as_ref().filter(|_| !self.nam_opts.freepats_di);
            return hybrid.or(self.banks[BankId::GuitarDirect.index()].as_ref());
        }
        self.banks[BankId::for_tone(tone)?.index()].as_ref()
    }

    fn is_double_tracked(&self, track: usize) -> bool {
        self.nam_opts.double_track && self.chains.get(track).is_some_and(|c| c.is_nam())
    }

    fn make_chain(&self, track: usize) -> AmpChain {
        let tone = self.tones[track];
        let bank = self.track_bank(track);
        if tone == ToneKind::Nam {
            if let (Some(model), Some(_)) = (&self.nam, bank) {
                return AmpChain::with_nam(model, self.sr, self.nam_opts.double_track);
            }
        }
        let processed = bank.is_some_and(|b| b.id.is_processed());
        AmpChain::new(tone, self.sr, processed)
    }

    fn seek(&mut self, tick: u64) {
        self.pos = tick as f64;
        for v in &mut self.voices {
            v.choke();
        }
        for v in &mut self.svoices {
            v.choke();
        }
        if let Some(s) = &self.sched {
            self.next_ev = s.events.partition_point(|e| e.start < tick);
            self.next_click = s.clicks.partition_point(|c| c.0 < tick);
            self.tempo_idx = s.timeline.tempo_map.partition_point(|e| e.0 <= tick).saturating_sub(1);
        }
        self.shared.set_pos(self.pos);
    }

    fn alloc_string_voice(&mut self) -> usize {
        if let Some(i) = self.voices.iter().position(|v| !v.is_active()) {
            return i;
        }
        // Steal: prefer choked/released voices, then the quietest.
        let mut best = 0;
        let mut best_score = f32::MAX;
        for (i, v) in self.voices.iter().enumerate() {
            let bias = match v.state {
                State::Choked => 0.0,
                State::Released => 0.5,
                _ => 1.0,
            };
            let score = bias + v.level();
            if score < best_score {
                best_score = score;
                best = i;
            }
        }
        best
    }

    fn alloc_sample_voice(&mut self) -> usize {
        if let Some(i) = self.svoices.iter().position(|v| !v.is_active()) {
            return i;
        }
        let mut best = 0;
        let mut best_score = f32::MAX;
        for (i, v) in self.svoices.iter().enumerate() {
            let bias = match v.state {
                State::Choked => 0.0,
                State::Released => 0.5,
                _ => 1.0,
            };
            let score = bias + v.level() - (self.age - v.age).min(1000) as f32 * 1e-4;
            if score < best_score {
                best_score = score;
                best = i;
            }
        }
        best
    }

    /// Play an event from a sampled bank. Returns false if the bank has no matching sample.
    fn trigger_sampled(&mut self, idx: usize, ev: &crate::timeline::NoteEvent, bank: Arc<Bank>) -> bool {
        if ev.drum {
            let Some(key) = gm_to_kit(ev.pitch as i32, self.alt_kick) else { return false };
            if key == 48 || key == 49 {
                self.alt_kick = !self.alt_kick;
            }
            // Closed / pedal hi-hat chokes a ringing open hi-hat.
            if key == 52 {
                for v in self.svoices.iter_mut().filter(|v| v.is_active() && v.drum && v.track == ev.track && v.key == 53) {
                    v.release_with(0.04, self.sr);
                }
            }
            let slot = self.alloc_sample_voice();
            let mut e = ev.clone();
            if ev.pitch as i32 == 44 {
                e.velocity *= 0.7;
            }
            return self.svoices[slot].start(idx, &e, &bank, key, self.sr, &mut self.rng, self.age, Take::default());
        }
        let on_string =
            |v: &SampleVoice| matches!(v.state, State::Sounding | State::Released) && !v.drum && v.track == ev.track && v.string == ev.string;
        if ev.legato && self.svoices.iter().any(|v| on_string(v) && v.level() > 1e-3) {
            for v in self.svoices.iter_mut().filter(|v| on_string(v)) {
                v.legato(idx, ev, &bank, self.sr);
            }
            return true;
        }
        for v in self.svoices.iter_mut().filter(|v| on_string(v)) {
            v.choke();
        }
        for v in self.voices.iter_mut().filter(|v| v.is_active() && v.track == ev.track && v.string == ev.string) {
            v.choke();
        }
        let slot = self.alloc_sample_voice();
        let mut e = ev.clone();
        if ev.legato {
            // Legato note without a sounding predecessor: play it softly.
            e.velocity *= 0.6;
        }
        let key = (ev.pitch + ev.curve_at(0.0)).round() as i32;
        let rr = (self.rng.next().abs() * 1024.0) as u32;
        if !self.is_double_tracked(ev.track) {
            return self.svoices[slot].start(idx, &e, &bank, key, self.sr, &mut self.rng, self.age, Take { rr, ..Take::default() });
        }
        // Double-tracked: a second, slightly different performance on the other side.
        let left = Take { rr, gl: 1.0, gr: 0.0, delay: 0, detune: 0.0 };
        let right = Take {
            rr: rr + 1,
            gl: 0.0,
            gr: 1.0,
            delay: (self.rng.next().abs() * 0.008 * self.sr) as u32,
            detune: self.rng.next() * 0.04,
        };
        let ok = self.svoices[slot].start(idx, &e, &bank, key, self.sr, &mut self.rng, self.age, left);
        let slot2 = self.alloc_sample_voice();
        e.velocity = (e.velocity * (1.0 + self.rng.next() * 0.08)).min(1.0);
        self.svoices[slot2].start(idx, &e, &bank, key, self.sr, &mut self.rng, self.age, right) || ok
    }

    fn trigger(&mut self, sched: &Schedule, idx: usize) {
        let ev = &sched.events[idx];
        self.age += 1;
        if let Some(bank) = self.track_bank(ev.track).cloned() {
            if self.trigger_sampled(idx, ev, bank) {
                return;
            }
        }
        if ev.drum {
            let key = ev.pitch as i32;
            let slot = self
                .drums
                .iter()
                .position(|d| !d.active)
                .unwrap_or_else(|| self.drums.iter().enumerate().min_by_key(|(_, d)| d.age).map(|(i, _)| i).unwrap_or(0));
            let is_closing_hat = key == 42 || key == 44;
            if is_closing_hat {
                for d in self.drums.iter_mut().filter(|d| d.active && d.track == ev.track) {
                    d.choke();
                }
            }
            self.drums[slot].trigger(ev.track, key, ev.velocity, self.sr, &mut self.rng, self.age);
            return;
        }
        let existing = self
            .voices
            .iter()
            .position(|v| matches!(v.state, State::Sounding | State::Released) && v.track == ev.track && v.string == ev.string);
        if ev.legato {
            if let Some(i) = existing {
                if self.voices[i].level() > 1e-3 {
                    self.voices[i].legato(idx, ev);
                    return;
                }
            }
        }
        if let Some(i) = existing {
            self.voices[i].choke();
        }
        let slot = self.alloc_string_voice();
        let tone = self.tones.get(ev.track).copied().unwrap_or(ToneKind::Clean);
        // Legato notes without a sounding predecessor are played softly.
        let mut ev_local_vel = ev.velocity;
        if ev.legato {
            ev_local_vel *= 0.6;
        }
        let v = &mut self.voices[slot];
        if (ev_local_vel - ev.velocity).abs() > f32::EPSILON {
            let mut e = ev.clone();
            e.velocity = ev_local_vel;
            v.pluck(idx, &e, tone, self.sr, &mut self.rng, self.age);
        } else {
            v.pluck(idx, ev, tone, self.sr, &mut self.rng, self.age);
        }
    }

    fn start_click(&mut self, accent: bool) {
        self.click_t = 0.0;
        self.click_freq = if accent { 1760.0 } else { 1320.0 };
        self.click_amp = if accent { 0.35 } else { 0.25 };
    }

    /// Render interleaved audio.
    pub fn process(&mut self, out: &mut [f32], channels: usize) {
        if let Some(rx) = self.rx.take() {
            while let Ok(cmd) = rx.try_recv() {
                self.handle(cmd);
            }
            self.rx = Some(rx);
        }
        let Some(sched) = self.sched.clone() else {
            out.fill(0.0);
            return;
        };
        for block in out.chunks_mut(BLOCK * channels) {
            self.process_block(&sched, block, channels);
        }
        self.shared.set_pos(self.pos);
        self.shared.playing.store(self.playing || self.count_in_left > 0.0, Ordering::Relaxed);
    }

    /// Advance the transport by one frame, triggering due events.
    fn tick(&mut self, sched: &Schedule) {
        let tempo_map = &sched.timeline.tempo_map;
        let bpm = tempo_map.get(self.tempo_idx).map(|e| e.1).unwrap_or(120.0);
        let dt = bpm / 60.0 * TPQ as f64 / self.sr as f64 * self.speed;
        if self.count_in_left > 0.0 {
            let before = self.count_in_left;
            if (before - TPQ as f64 * 4.0).abs() < 1e-9 {
                self.start_click(true);
            }
            self.count_in_left -= dt;
            if self.count_in_left > 0.0 && (before / TPQ as f64).ceil() != (self.count_in_left / TPQ as f64).ceil() {
                self.start_click(false);
            }
            return;
        }
        while self.next_ev < sched.events.len() && sched.events[self.next_ev].start as f64 <= self.pos {
            let i = self.next_ev;
            self.next_ev += 1;
            self.trigger(sched, i);
        }
        while self.next_click < sched.clicks.len() && sched.clicks[self.next_click].0 as f64 <= self.pos {
            let accent = sched.clicks[self.next_click].1;
            self.next_click += 1;
            if self.metronome {
                self.start_click(accent);
            }
        }
        self.pos += dt;
        while self.tempo_idx + 1 < tempo_map.len() && tempo_map[self.tempo_idx + 1].0 as f64 <= self.pos {
            self.tempo_idx += 1;
        }
        if let Some((a, b)) = self.loop_range {
            if self.pos >= b as f64 {
                self.seek(a);
            }
        }
        if self.pos >= sched.timeline.total as f64 {
            self.playing = false;
            for v in &mut self.voices {
                v.release();
            }
            for v in self.svoices.iter_mut().filter(|v| !v.drum) {
                v.release();
            }
        }
    }

    fn process_block(&mut self, sched: &Schedule, out: &mut [f32], channels: usize) {
        let sr = self.sr;
        let n = out.len() / channels;
        let n_tracks = sched.tracks.len().min(self.track_buf.len());
        for b in &mut self.track_buf {
            b[0][..n].fill(0.0);
            b[1][..n].fill(0.0);
        }

        // Voices, frame by frame, into per-track dry buffers.
        for i in 0..n {
            if self.playing {
                self.tick(sched);
            }
            let pos = self.pos;
            for v in self.voices.iter_mut().filter(|v| v.is_active()) {
                let ev = &sched.events[v.ev];
                if v.state == State::Sounding && (pos >= ev.end as f64 || !self.playing) {
                    v.release();
                }
                let s = v.process(ev, pos - ev.start as f64, sr);
                if let Some(b) = self.track_buf.get_mut(v.track) {
                    b[0][i] += s;
                    b[1][i] += s;
                }
            }
            for d in self.drums.iter_mut().filter(|d| d.active) {
                let s = d.process(sr, &mut self.rng);
                if let Some(b) = self.track_buf.get_mut(d.track) {
                    b[0][i] += s;
                    b[1][i] += s;
                }
            }
            for v in self.svoices.iter_mut().filter(|v| v.is_active()) {
                let Some(bank) = self.banks[v.bank].as_deref() else {
                    v.choke();
                    continue;
                };
                let ev = &sched.events[v.ev];
                if !v.drum && v.state == State::Sounding && (pos >= ev.end as f64 || !self.playing) {
                    v.release();
                }
                let (sl, sr_) = v.process(ev, pos - ev.start as f64, sr, bank);
                if let Some(b) = self.track_buf.get_mut(v.track) {
                    b[0][i] += sl;
                    b[1][i] += sr_;
                }
            }
            self.click_buf[i] = if self.click_t < 0.05 {
                let c = (self.click_t * self.click_freq * std::f32::consts::TAU).sin()
                    * (-self.click_t / 0.012).exp()
                    * self.click_amp;
                self.click_t += 1.0 / sr;
                c
            } else {
                0.0
            };
        }

        // Amp / cab per track.
        let any_solo = self.mix.iter().any(|m| m.solo);
        let audible: Vec<bool> = self.mix.iter().map(|m| !m.mute && (!any_solo || m.solo)).collect();
        for t in 0..n_tracks {
            let [bl, br] = &mut self.track_buf[t];
            // Neural amp models are expensive: skip them for tracks nobody hears.
            if self.chains[t].is_nam() && !audible[t] {
                continue;
            }
            self.chains[t].process_block(&mut bl[..n], &mut br[..n]);
        }

        // Mix.
        for (i, frame) in out.chunks_mut(channels).enumerate() {
            let (mut l, mut r) = (0.0f32, 0.0f32);
            let mut send = 0.0;
            for t in 0..n_tracks {
                if !audible[t] {
                    continue;
                }
                let (yl, yr) = (self.track_buf[t][0][i], self.track_buf[t][1][i]);
                let g = self.mix[t].volume * 1.4;
                let pan = sched.tracks[t].pan;
                let gl = ((1.0 - pan) * 0.5).sqrt();
                let gr = ((1.0 + pan) * 0.5).sqrt();
                l += yl * g * gl;
                r += yr * g * gr;
                send += 0.5 * (yl + yr) * g * self.chains[t].reverb_send;
            }
            let (rl, rr) = self.reverb.process(send);
            let c = self.click_buf[i];
            let l = soft_limit((l + rl + c) * self.master);
            let r = soft_limit((r + rr + c) * self.master);
            match channels {
                1 => frame[0] = 0.5 * (l + r),
                _ => {
                    frame[0] = l;
                    frame[1] = r;
                    for x in frame.iter_mut().skip(2) {
                        *x = 0.0;
                    }
                }
            }
        }
    }
}

