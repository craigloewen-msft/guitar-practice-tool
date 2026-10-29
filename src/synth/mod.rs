//! Real-time playback engine: schedules note events into string/drum voices and mixes tracks.

pub mod amp;
pub mod drums;
pub mod dsp;
pub mod nam;
pub mod reverb;
pub mod sampler;
pub mod stretch;
pub mod string;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::backing::{BackingSet, MAX_STEMS};
use crate::model::{ToneKind, TPQ};
use crate::timeline::Schedule;
use amp::AmpChain;
use drums::DrumVoice;
use dsp::{soft_limit, Rng};
use reverb::Reverb;
use nam::NamModel;
use sampler::{gm_to_kit, Bank, BankId, SampleVoice};
use stretch::Stretcher;
use string::{State, StringVoice, Take};

const MAX_STRING_VOICES: usize = 96;
const MAX_DRUM_VOICES: usize = 32;
const MAX_SAMPLE_VOICES: usize = 128;
/// Frames rendered per block (amp models process blocks).
const BLOCK: usize = 256;

/// How guitars on the "NAM drive" tone are produced.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NamOptions {
    /// Two performances panned hard left/right, each through its own amp.
    pub double_track: bool,
}

impl Default for NamOptions {
    fn default() -> Self {
        NamOptions { double_track: true }
    }
}

/// What the transport plays back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Source {
    /// The synthesised tab.
    #[default]
    Synth,
    /// The attached recording, followed in sync with the tab.
    Song,
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
    /// Neural Amp Modeler capture that powers every guitar tone.
    Nam(Option<Arc<NamModel>>),
    NamOptions(NamOptions),
    /// Attach (or drop) a set of backing stems lined up with the score.
    Backing(Option<Arc<BackingSet>>),
    /// Listen to the synth or to the recording.
    Source(Source),
    /// 0..1 level for the recording as a whole.
    BackingVolume(f32),
    /// Hand adjustment to the alignment, in seconds. Positive plays the recording later.
    Trim(f64),
    /// 0..1 level for one stem, already folded together with its mute and any solo.
    StemGain(usize, f32),
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
    backing: Option<Arc<BackingSet>>,
    source: Source,
    backing_volume: f32,
    /// Hand adjustment to the alignment, in seconds.
    backing_trim: f64,
    /// Per-stem levels: what the mixer asks for, and what is currently being applied.
    stem_gain: [f32; MAX_STEMS],
    stem_cur: [f32; MAX_STEMS],
    /// Smoothed gain, so starting and stopping the recording doesn't click.
    back_gain: f32,
    /// Position in the recording reached on the previous frame, in seconds.
    back_t: f64,
    /// Crossfade out of the pre-seek position: (time, frames left).
    back_fade: Option<(f64, f32)>,
    /// In Song mode the recording is the clock: its position in seconds (before the trim),
    /// advancing at a steady rate while the tab follows. NaN until it has been taken from the
    /// transport, which happens again after every seek or change of source.
    clock: f64,
    back_buf: [Vec<f32>; 2],
    /// Plays the recording slower or faster than it was made without changing its pitch.
    stretch: Stretcher,
    /// How much of the stretched recording is heard rather than the plain read (0–1), so moving
    /// between the two doesn't click.
    stretch_mix: f32,
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
            backing: None,
            source: Source::Synth,
            backing_volume: 0.9,
            backing_trim: 0.0,
            stem_gain: [1.0; MAX_STEMS],
            stem_cur: [0.0; MAX_STEMS],
            back_gain: 0.0,
            back_t: f64::NAN,
            clock: f64::NAN,
            back_fade: None,
            back_buf: [vec![0.0; BLOCK], vec![0.0; BLOCK]],
            stretch: Stretcher::new(sr),
            stretch_mix: 0.0,
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
                // With a loop active, pausing rewinds to its start so the next play
                // run begins at the top of the loop. Voices keep their release tail
                // (unlike `seek`, which chokes them).
                if let Some((a, _)) = self.loop_range {
                    self.locate_at(a);
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
            Command::Backing(b) => {
                self.backing = b;
                self.back_t = f64::NAN;
                self.clock = f64::NAN;
                self.back_fade = None;
                self.stretch.stop();
            }
            Command::Source(s) => {
                if self.source != s {
                    self.source = s;
                    self.clock = f64::NAN;
                    // Anything already ringing belongs to the other source.
                    for v in &mut self.voices {
                        v.release();
                    }
                    for v in &mut self.svoices {
                        v.release_with(if v.drum { 0.15 } else { 0.07 }, self.sr);
                    }
                }
            }
            Command::BackingVolume(v) => self.backing_volume = v.clamp(0.0, 2.0),
            // The read position jumps; the jump check in `process_block` fades across it.
            Command::Trim(t) => self.backing_trim = t.clamp(-5.0, 5.0),
            Command::StemGain(i, v) => {
                if let Some(g) = self.stem_gain.get_mut(i) {
                    *g = v.clamp(0.0, 2.0);
                }
            }
        }
    }

    fn rebuild_nam_chains(&mut self) {
        for i in 0..self.tones.len() {
            if matches!(self.tones[i], ToneKind::Nam | ToneKind::Clean) {
                for v in self.voices.iter_mut().filter(|v| v.track == i) {
                    v.choke();
                }
                self.chains[i] = self.make_chain(i);
            }
        }
    }

    /// Loaded sound bank for a track's tone (bass and drums only), if any.
    fn track_bank(&self, track: usize) -> Option<&Arc<Bank>> {
        let tone = *self.tones.get(track)?;
        self.banks[BankId::for_tone(tone)?.index()].as_ref()
    }

    fn is_double_tracked(&self, track: usize) -> bool {
        self.nam_opts.double_track && self.tones.get(track) == Some(&ToneKind::Nam) && self.chains.get(track).is_some_and(|c| c.is_nam())
    }

    fn make_chain(&self, track: usize) -> AmpChain {
        let tone = self.tones[track];
        let bank = self.track_bank(track);
        if let (ToneKind::Nam | ToneKind::Clean, Some(model)) = (tone, &self.nam) {
            return AmpChain::with_nam(model, self.sr, tone, self.nam_opts.double_track);
        }
        let processed = bank.is_some_and(|b| b.id.is_processed());
        AmpChain::new(tone, self.sr, processed)
    }

    fn seek(&mut self, tick: u64) {
        for v in &mut self.voices {
            v.choke();
        }
        for v in &mut self.svoices {
            v.choke();
        }
        self.locate_at(tick);
    }

    /// Move the transport without touching sounding voices.
    fn locate_at(&mut self, tick: u64) {
        self.pos = tick as f64;
        self.clock = f64::NAN;
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
            return self.svoices[slot].start(idx, &e, &bank, key, self.sr, &mut self.rng, self.age);
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
        self.svoices[slot].start(idx, &e, &bank, key, self.sr, &mut self.rng, self.age)
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
        // Every take of a double-tracked part follows the same string.
        let on_string =
            |v: &StringVoice| matches!(v.state, State::Sounding | State::Released) && v.track == ev.track && v.string == ev.string;
        if ev.legato && self.voices.iter().any(|v| on_string(v) && v.level() > 1e-3) {
            for v in self.voices.iter_mut().filter(|v| on_string(v)) {
                v.legato(idx, ev);
            }
            return;
        }
        for v in self.voices.iter_mut().filter(|v| on_string(v)) {
            v.choke();
        }
        let tone = self.tones.get(ev.track).copied().unwrap_or(ToneKind::Clean);
        let mut e = ev.clone();
        if ev.legato {
            // Legato note without a sounding predecessor: play it softly.
            e.velocity *= 0.6;
        }
        if !self.is_double_tracked(ev.track) {
            let slot = self.alloc_string_voice();
            self.voices[slot].pluck(idx, &e, tone, self.sr, &mut self.rng, self.age, Take::default());
            return;
        }
        // Double-tracked: a second, slightly different performance on the other side.
        let left = Take { gl: 1.0, gr: 0.0, delay: 0, detune: 0.0 };
        let right = Take {
            gl: 0.0,
            gr: 1.0,
            delay: (self.rng.next().abs() * 0.008 * self.sr) as u32,
            detune: self.rng.next() * 0.05,
        };
        let slot = self.alloc_string_voice();
        self.voices[slot].pluck(idx, &e, tone, self.sr, &mut self.rng, self.age, left);
        let slot2 = self.alloc_string_voice();
        e.velocity = (e.velocity * (1.0 + self.rng.next() * 0.08)).min(1.0);
        self.voices[slot2].pluck(idx, &e, tone, self.sr, &mut self.rng, self.age, right);
    }

    fn start_click(&mut self, accent: bool) {
        self.click_t = 0.0;
        self.click_freq = if accent { 1760.0 } else { 1320.0 };
        self.click_amp = if accent { 0.35 } else { 0.25 };
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr
    }

    /// One-line snapshot of engine load, for `GPT_TRACE`.
    pub fn trace_line(&self) -> String {
        let any_solo = self.mix.iter().any(|m| m.solo);
        let nam = (0..self.chains.len())
            .filter(|&t| self.chains[t].is_nam() && self.mix.get(t).is_some_and(|m| !m.mute && (!any_solo || m.solo)))
            .count();
        format!(
            "playing={} pos={:.0} speed={:.2} strings={} samples={} nam_chains={} sched={}",
            self.playing,
            self.pos,
            self.speed,
            self.voices.iter().filter(|v| v.is_active()).count(),
            self.svoices.iter().filter(|v| v.is_active()).count(),
            nam,
            self.sched.is_some(),
        )
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

    /// The recording keeping time, when it is the one being heard and it lined up.
    fn song_clock(&self) -> Option<Arc<BackingSet>> {
        if self.source != Source::Song {
            return None;
        }
        self.backing.as_ref().filter(|b| b.align.ok()).cloned()
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
            // In song mode the recording carries the notes; the metronome still runs.
            if self.source == Source::Synth {
                self.trigger(sched, i);
            }
        }
        while self.next_click < sched.clicks.len() && sched.clicks[self.next_click].0 as f64 <= self.pos {
            let accent = sched.clicks[self.next_click].1;
            self.next_click += 1;
            if self.metronome {
                self.start_click(accent);
            }
        }
        match self.song_clock() {
            // The recording plays at a steady rate and never bends; the tab is carried along to
            // wherever the alignment says the recording has got to. It never steps backwards,
            // so a note can't be triggered twice.
            Some(b) => {
                if !self.clock.is_finite() {
                    self.clock = b.audio_time(sched.timeline.seconds_at(self.pos));
                }
                self.clock += self.speed / self.sr as f64;
                let to = sched.timeline.tick_at_seconds(b.score_time(self.clock));
                self.pos = to.max(self.pos);
            }
            None => self.pos += dt,
        }
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
        // Read by position, so looping, seeking and speed changes carry the recording along.
        let backing = self.backing.clone();
        let back_target = if self.source == Source::Song && self.playing && self.count_in_left <= 0.0 {
            self.backing_volume
        } else {
            0.0
        };
        let fade_len = (sr * 0.006).max(1.0);
        let gain_step = 1.0 / (sr * 0.01);

        // Voices, frame by frame, into per-track dry buffers.
        for i in 0..n {
            if self.playing {
                self.tick(sched);
            }
            let pos = self.pos;
            match &backing {
                Some(b) => {
                    // The clock only runs in Song mode with a lined-up recording.
                    let (t, step) = if self.clock.is_finite() {
                        (self.clock + self.backing_trim, self.speed / sr as f64)
                    } else {
                        let t = b.audio_time(sched.timeline.seconds_at(pos)) + self.backing_trim;
                        (t, self.speed * b.align.rate / sr as f64)
                    };
                    // A jump (seek, loop wrap) would click, so fade out of the old position.
                    if self.back_t.is_finite() && (t - self.back_t - step).abs() > 0.002 {
                        self.back_fade = Some((self.back_t, fade_len));
                    }
                    self.back_t = t;
                    let n_stems = b.stems.len().min(MAX_STEMS);
                    for (g, want) in self.stem_cur.iter_mut().zip(self.stem_gain).take(n_stems) {
                        *g += (want - *g).clamp(-gain_step, gain_step);
                    }
                    // Played at any rate but its own, the recording is time-stretched so it keeps
                    // its pitch; a plain read at a different rate would shift it.
                    let rate = step * sr as f64;
                    let stretched = if (rate - 1.0).abs() > 0.002 { 1.0 } else { 0.0 };
                    self.stretch_mix += (stretched - self.stretch_mix).clamp(-gain_step, gain_step);
                    // Every stem shares one time scale, so they are all read at the same point.
                    let fade = self.back_fade;
                    let (mut l, mut r) = (0.0f32, 0.0f32);
                    if self.stretch_mix < 1.0 {
                        for (si, s) in b.stems.iter().enumerate().take(n_stems) {
                            let g = self.stem_cur[si];
                            if g <= 0.0 {
                                continue;
                            }
                            let (mut sl, mut sr) = s.sample_at(t);
                            if let Some((ft, left)) = fade {
                                let (fl, fr) = s.sample_at(ft);
                                let k = left / fade_len;
                                sl += (fl - sl) * k;
                                sr += (fr - sr) * k;
                            }
                            l += sl * g;
                            r += sr * g;
                        }
                    }
                    let audible = back_target > 0.0 || self.back_gain > 0.0;
                    if self.stretch_mix > 0.0 && audible {
                        // While paused the transport stands still, so the stretch carries on by
                        // itself until it has faded out.
                        let now = (self.playing && self.count_in_left <= 0.0).then_some(t);
                        let gains = &self.stem_cur;
                        let (xl, xr) = self.stretch.next(now, rate, |start, dt, ol, or| {
                            ol.fill(0.0);
                            or.fill(0.0);
                            for (s, &g) in b.stems.iter().zip(gains).take(n_stems) {
                                if g <= 0.0 {
                                    continue;
                                }
                                for (i, (a, c)) in ol.iter_mut().zip(or.iter_mut()).enumerate() {
                                    let (x, y) = s.sample_at(start + i as f64 * dt);
                                    *a += x * g;
                                    *c += y * g;
                                }
                            }
                        });
                        l += (xl - l) * self.stretch_mix;
                        r += (xr - r) * self.stretch_mix;
                    } else {
                        self.stretch.stop();
                    }
                    if let Some((ft, left)) = &mut self.back_fade {
                        *ft += step;
                        *left -= 1.0;
                        if *left <= 0.0 {
                            self.back_fade = None;
                        }
                    }
                    self.back_gain += (back_target - self.back_gain).clamp(-gain_step, gain_step);
                    self.back_buf[0][i] = l * self.back_gain;
                    self.back_buf[1][i] = r * self.back_gain;
                }
                None => {
                    self.back_gain = 0.0;
                    self.back_buf[0][i] = 0.0;
                    self.back_buf[1][i] = 0.0;
                }
            }
            for v in self.voices.iter_mut().filter(|v| v.is_active()) {
                let ev = &sched.events[v.ev];
                if v.state == State::Sounding && (pos >= ev.end as f64 || !self.playing) {
                    v.release();
                }
                let s = v.process(ev, pos - ev.start as f64, sr);
                if let Some(b) = self.track_buf.get_mut(v.track) {
                    b[0][i] += s * v.take.gl;
                    b[1][i] += s * v.take.gr;
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
        // In song mode nothing new is triggered, so once the last tail has died the synth
        // tracks can be skipped entirely instead of grinding amp models over silence.
        let synth_on = self.source == Source::Synth
            || self.voices.iter().any(|v| v.is_active())
            || self.svoices.iter().any(|v| v.is_active())
            || self.drums.iter().any(|d| d.active);
        let audible: Vec<bool> = self.mix.iter().map(|m| synth_on && !m.mute && (!any_solo || m.solo)).collect();
        // Neural amp models are expensive: skip them for tracks nobody hears, and run the
        // rest in parallel (each track's chain is independent).
        use rayon::prelude::*;
        self.chains[..n_tracks]
            .par_iter_mut()
            .zip(&mut self.track_buf[..n_tracks])
            .zip(&audible[..n_tracks])
            .filter(|((c, _), a)| **a || !c.is_nam())
            .for_each(|((chain, [bl, br]), _)| chain.process_block(&mut bl[..n], &mut br[..n]));

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
            let l = soft_limit((l + rl + c + self.back_buf[0][i]) * self.master);
            let r = soft_limit((r + rr + c + self.back_buf[1][i]) * self.master);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> Engine {
        Engine::new(48_000.0, Arc::new(Shared::default()), None)
    }

    #[test]
    fn pause_rewinds_to_the_loop_start() {
        let mut e = engine();
        e.handle(Command::Loop(Some((1920, 7680))));
        e.handle(Command::Seek(5000));
        e.handle(Command::Play);
        e.pos = 5500.0;
        e.handle(Command::Pause);
        assert!(!e.playing, "pause should stop playback");
        assert_eq!(e.pos as u64, 1920, "pause inside a loop should rewind to its start");
    }

    #[test]
    fn pause_without_a_loop_stays_put() {
        let mut e = engine();
        e.handle(Command::Play);
        e.pos = 5500.0;
        e.handle(Command::Pause);
        assert_eq!(e.pos as u64, 5500, "pause without a loop should hold the position");
    }

    /// Twenty bars of 4/4 at 120bpm with no notes.
    fn empty_sched() -> Arc<Schedule> {
        use crate::model::{MeasureHeader, Song};
        use crate::timeline::Timeline;

        let mut song = Song { tempo: 120, ..Default::default() };
        for i in 0..20 {
            song.measure_headers.push(MeasureHeader {
                number: i + 1,
                start: i as u32 * 4 * TPQ,
                numerator: 4,
                denominator: 4,
                repeat_close: -1,
                ..Default::default()
            });
        }
        Arc::new(Schedule { events: vec![], clicks: vec![], timeline: Timeline::build(&song), tracks: vec![] })
    }

    /// A good alignment following `map` (tab seconds, recording seconds).
    fn aligned(map: Vec<(f64, f64)>) -> crate::backing::Alignment {
        use crate::backing::{AlignStatus, Alignment};
        Alignment {
            offset: map[0].1 - map[0].0,
            rate: 1.0,
            confidence: 1.0,
            hit_error: 0.0,
            hit_part: String::new(),
            checks: vec![],
            max_drift: 0.0,
            status: AlignStatus::Good,
            summary: String::new(),
            detail: String::new(),
            map,
        }
    }

    /// In Song mode the recording sets the pace: it is read at exactly one second per second
    /// however the band's tempo differs from the tab, and the tab is carried along to match.
    #[test]
    fn song_mode_lets_the_recording_keep_time() {
        let sched = empty_sched();
        // Bar 1 five seconds in, then the band 25% slower than the tab for the first 10s of
        // the tab and at the tab's tempo after that.
        let align = aligned(vec![(0.0, 5.0), (10.0, 17.5), (40.0, 47.5)]);
        let set = Arc::new(BackingSet { stems: vec![], align });

        let mut e = engine();
        e.handle(Command::SetSchedule(sched.clone()));
        e.handle(Command::Backing(Some(set)));
        e.handle(Command::Source(Source::Song));
        e.handle(Command::Seek(0));
        e.handle(Command::Play);
        let sr = e.sr as usize;
        let tl = &sched.timeline;
        for _ in 0..sr * 2 {
            e.tick(&sched);
        }
        // Two seconds of recording from 5.0s is 1.6s of tab in the slow stretch.
        assert!((e.clock - 7.0).abs() < 1e-6, "recording at {}", e.clock);
        assert!((tl.seconds_at(e.pos) - 1.6).abs() < 0.001, "tab at {}s", tl.seconds_at(e.pos));

        // Into the stretch at tab tempo, the two move together.
        e.handle(Command::Seek(tl.tick_at_seconds(20.0) as u64));
        let from = tl.seconds_at(e.pos);
        for _ in 0..sr {
            e.tick(&sched);
        }
        assert!((e.clock - 28.5).abs() < 1e-6, "recording at {} after a seek", e.clock);
        assert!((tl.seconds_at(e.pos) - from - 1.0).abs() < 0.001, "tab moved {}s", tl.seconds_at(e.pos) - from);

        // The synth keeps the tab's own time.
        e.handle(Command::Source(Source::Synth));
        let from = tl.seconds_at(e.pos);
        for _ in 0..sr {
            e.tick(&sched);
        }
        assert!((tl.seconds_at(e.pos) - from - 1.0).abs() < 0.001);
    }

    /// Slowing the song down slows the recording without dropping its pitch.
    #[test]
    fn slowing_the_recording_keeps_its_pitch() {
        use crate::backing::Stem;
        use crate::synth::string::tests::estimate_hz;

        let sr = 48_000u32;
        let samples: Vec<i16> = (0..sr as usize * 20)
            .flat_map(|i| {
                let v = (0.3 * (std::f64::consts::TAU * 220.0 * i as f64 / sr as f64).sin() * 32767.0) as i16;
                [v, v]
            })
            .collect();

        for speed in [1.0, 0.5, 0.8] {
            let stem = Stem::from_samples(sr, samples.clone());
            let set = BackingSet { stems: vec![stem], align: aligned(vec![(0.0, 0.0), (40.0, 40.0)]) };
            let mut e = engine();
            e.handle(Command::SetSchedule(empty_sched()));
            e.handle(Command::Backing(Some(Arc::new(set))));
            e.handle(Command::Source(Source::Song));
            e.handle(Command::Speed(speed));
            // Two seconds in.
            e.handle(Command::Seek(TPQ as u64 * 4));
            e.handle(Command::Play);
            let mut out = vec![0.0; sr as usize * 2];
            e.process(&mut out, 2);
            let left: Vec<f32> = out.iter().step_by(2).skip(sr as usize / 10).copied().collect();
            let hz = estimate_hz(&left, sr as f32);
            assert!((hz - 220.0).abs() < 1.0, "speed {speed}: heard {hz} Hz");
            // The recording still moves at the slower pace.
            assert!((e.clock - 2.0 - speed).abs() < 1e-3, "speed {speed}: recording at {}", e.clock);
        }
    }
}
