//! Physically-inspired plucked string (extended Karplus-Strong).

use super::dsp::{flush, midi_to_hz, Rng};
use crate::model::ToneKind;
use crate::timeline::NoteEvent;

const BUF: usize = 8192;
const MASK: usize = BUF - 1;

#[derive(Clone, Copy)]
pub struct StringParams {
    /// Loop lowpass coefficient (0 = bright, 0.9 = very dark).
    pub loop_a: f32,
    /// Sustain T60 in seconds at MIDI 40 and MIDI 88.
    pub t60_low: f32,
    pub t60_high: f32,
    /// Pickup position as a fraction of the string length from the bridge.
    pub pickup: f32,
    /// Pick position as a fraction of the string length from the bridge.
    pub pick: f32,
    /// Excitation lowpass coefficient at full velocity (lower = brighter pick).
    pub pick_dark: f32,
}

impl StringParams {
    pub fn for_tone(tone: ToneKind) -> Self {
        match tone {
            ToneKind::Acoustic => StringParams { loop_a: 0.06, t60_low: 5.0, t60_high: 1.6, pickup: 0.0, pick: 0.17, pick_dark: 0.15 },
            ToneKind::Clean => StringParams { loop_a: 0.10, t60_low: 6.0, t60_high: 2.0, pickup: 0.22, pick: 0.15, pick_dark: 0.25 },
            ToneKind::Crunch | ToneKind::HighGain => {
                StringParams { loop_a: 0.10, t60_low: 7.0, t60_high: 2.5, pickup: 0.11, pick: 0.13, pick_dark: 0.2 }
            }
            ToneKind::Bass => StringParams { loop_a: 0.32, t60_low: 5.0, t60_high: 2.0, pickup: 0.18, pick: 0.2, pick_dark: 0.55 },
            ToneKind::Drums => StringParams { loop_a: 0.2, t60_low: 1.0, t60_high: 1.0, pickup: 0.0, pick: 0.2, pick_dark: 0.3 },
        }
    }
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum State {
    Idle,
    Sounding,
    /// Note-off: string damped by the fretting hand.
    Released,
    /// Fast fade (replaced by a new note on the same string or a seek).
    Choked,
}

pub struct StringVoice {
    buf: Vec<f32>,
    w: usize,
    pub state: State,
    pub track: usize,
    pub string: usize,
    /// Index of the event controlling pitch/end.
    pub ev: usize,
    pub age: u64,
    params: StringParams,
    lp: f32,
    loop_a: f32,
    loop_a_target: f32,
    t60: f32,
    t60_target: f32,
    pitch: f32,
    gain_env: f32,
    level: f32,
    vib_phase: f32,
    samples: u64,
    pickup_on: bool,
    dc_x: f32,
    dc_y: f32,
}

impl StringVoice {
    pub fn new() -> Self {
        StringVoice {
            buf: vec![0.0; BUF],
            w: 0,
            state: State::Idle,
            track: 0,
            string: 0,
            ev: 0,
            age: 0,
            params: StringParams::for_tone(ToneKind::Clean),
            lp: 0.0,
            loop_a: 0.1,
            loop_a_target: 0.1,
            t60: 1.0,
            t60_target: 1.0,
            pitch: 60.0,
            gain_env: 1.0,
            level: 0.0,
            vib_phase: 0.0,
            samples: 0,
            pickup_on: true,
            dc_x: 0.0,
            dc_y: 0.0,
        }
    }

    pub fn is_active(&self) -> bool {
        self.state != State::Idle
    }

    pub fn level(&self) -> f32 {
        self.level * self.gain_env
    }

    fn sustain_t60(&self, ev: &NoteEvent) -> f32 {
        if ev.dead {
            return 0.035;
        }
        let k = ((ev.pitch - 40.0) / 48.0).clamp(-0.5, 1.5);
        let base = self.params.t60_low * (self.params.t60_high / self.params.t60_low).powf(k);
        if ev.palm_mute {
            (0.32 - 0.12 * k).clamp(0.12, 0.35)
        } else {
            base
        }
    }

    fn sustain_loop_a(&self, ev: &NoteEvent) -> f32 {
        if ev.dead {
            0.6
        } else if ev.palm_mute {
            (self.params.loop_a + 0.42).min(0.75)
        } else if ev.harmonic {
            self.params.loop_a * 0.5
        } else {
            self.params.loop_a
        }
    }

    /// Start a new plucked note.
    pub fn pluck(&mut self, ev_idx: usize, ev: &NoteEvent, tone: ToneKind, sr: f32, rng: &mut Rng, age: u64) {
        self.params = StringParams::for_tone(tone);
        self.state = State::Sounding;
        self.track = ev.track;
        self.string = ev.string;
        self.ev = ev_idx;
        self.age = age;
        self.gain_env = 1.0;
        self.level = 1.0;
        self.samples = 0;
        self.vib_phase = 0.0;
        self.lp = 0.0;
        self.dc_x = 0.0;
        self.dc_y = 0.0;
        self.pickup_on = !ev.harmonic && self.params.pickup > 0.0;
        self.loop_a = self.sustain_loop_a(ev);
        self.loop_a_target = self.loop_a;
        self.t60 = self.sustain_t60(ev);
        self.t60_target = self.t60;
        self.pitch = ev.pitch + ev.curve_at(0.0);
        self.buf.fill(0.0);

        let period = (sr / midi_to_hz(self.pitch)).clamp(4.0, (BUF - 4) as f32);
        let n = period.round() as usize;
        let vel = ev.velocity;
        // Lower velocity -> darker, softer pick attack.
        let mut c = (self.params.pick_dark + (1.0 - vel) * 0.45).clamp(0.0, 0.95);
        if ev.harmonic {
            c = 0.85;
        }
        if ev.dead {
            c = 0.3;
        }
        if ev.palm_mute {
            c = (c + 0.1).min(0.9);
        }
        let pick = (self.params.pick * (1.0 + 0.15 * rng.next())).clamp(0.05, 0.45);
        let pk = ((pick * n as f32).round() as usize).clamp(1, n.saturating_sub(1).max(1));
        let start = self.w.wrapping_sub(n) & MASK;
        let noise_mix = if ev.dead { 1.0 } else { 0.55 };
        let mut y = 0.0f32;
        for i in 0..n {
            // Initial string displacement: triangle with the apex at the pick point, plus noise.
            let tri = if i < pk { i as f32 / pk as f32 } else { (n - i) as f32 / (n - pk) as f32 };
            let x = (1.0 - noise_mix) * (tri * 2.0 - 1.0) + noise_mix * rng.next();
            y = (1.0 - c) * x + c * y;
            self.buf[(start + i) & MASK] = y;
        }
        // Pick-position comb (removes harmonics with a node at the pick point).
        for i in (pk..n).rev() {
            let a = self.buf[(start + i) & MASK];
            let b = self.buf[(start + i - pk) & MASK];
            self.buf[(start + i) & MASK] = a - b;
        }
        let mut mean = 0.0;
        for i in 0..n {
            mean += self.buf[(start + i) & MASK];
        }
        mean /= n as f32;
        let mut peak = 1e-6f32;
        for i in 0..n {
            let v = self.buf[(start + i) & MASK] - mean;
            self.buf[(start + i) & MASK] = v;
            peak = peak.max(v.abs());
        }
        let amp = vel.powf(1.3) / peak;
        for i in 0..n {
            self.buf[(start + i) & MASK] *= amp;
        }
    }

    /// Continue sounding with a new pitch target (hammer-on / pull-off / legato slide).
    pub fn legato(&mut self, ev_idx: usize, ev: &NoteEvent) {
        self.ev = ev_idx;
        self.state = State::Sounding;
        self.loop_a_target = self.sustain_loop_a(ev);
        self.t60_target = self.sustain_t60(ev);
        self.t60 = self.t60.max(self.t60_target * 0.5);
        self.vib_phase = 0.0;
    }

    pub fn release(&mut self) {
        if self.state == State::Sounding {
            self.state = State::Released;
            self.t60_target = if self.params.loop_a > 0.25 { 0.12 } else { 0.08 };
            self.loop_a_target = (self.loop_a + 0.25).min(0.7);
        }
    }

    pub fn choke(&mut self) {
        if self.state != State::Idle {
            self.state = State::Choked;
        }
    }

    /// Render one sample. `rel_ticks` is the playback position relative to the controlling event start.
    #[inline]
    pub fn process(&mut self, ev: &NoteEvent, rel_ticks: f64, sr: f32) -> f32 {
        // Smooth parameter changes.
        self.loop_a += (self.loop_a_target - self.loop_a) * 0.002;
        self.t60 += (self.t60_target - self.t60) * 0.01;

        let mut target = ev.pitch + ev.curve_at(rel_ticks.max(0.0));
        if let Some((vstart, depth)) = ev.vibrato {
            if rel_ticks >= vstart as f64 {
                self.vib_phase += 5.5 / sr;
                if self.vib_phase > 1.0 {
                    self.vib_phase -= 1.0;
                }
                // Guitar vibrato only bends upwards.
                target += depth * 0.5 * (1.0 - (self.vib_phase * std::f32::consts::TAU).cos());
            }
        }
        self.pitch += (target - self.pitch) * 0.02;

        let f = midi_to_hz(self.pitch);
        let w = std::f32::consts::TAU * f / sr;
        let a = self.loop_a;
        // Exact phase delay of the one-pole loop filter at the fundamental.
        let filt_delay = (a * w.sin()).atan2(1.0 - a * w.cos()) / w;
        let period = sr / f;
        let d = (period - filt_delay).clamp(3.0, (BUF - 4) as f32);

        // Hermite-interpolated read at fractional delay d.
        let rp = self.w as f32 - d + BUF as f32;
        let i = rp.floor();
        let frac = rp - i;
        let i = i as usize;
        let xm1 = self.buf[(i.wrapping_sub(1)) & MASK];
        let x0 = self.buf[i & MASK];
        let x1 = self.buf[(i + 1) & MASK];
        let x2 = self.buf[(i + 2) & MASK];
        let c1 = 0.5 * (x1 - xm1);
        let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
        let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
        let y = ((c3 * frac + c2) * frac + c1) * frac + x0;

        let g = 10f32.powf(-3.0 * period / (self.t60.max(0.005) * sr));
        self.lp = flush((1.0 - a) * y + a * self.lp);
        self.buf[self.w] = self.lp * g;
        self.w = (self.w + 1) & MASK;

        let mut out = y;
        if self.pickup_on {
            let tap = (d * self.params.pickup) as usize;
            out -= self.buf[(self.w.wrapping_sub(tap + 1)) & MASK];
            out *= 0.7;
        }
        // DC blocker
        let o = out - self.dc_x + 0.995 * self.dc_y;
        self.dc_x = out;
        self.dc_y = flush(o);

        if self.state == State::Choked {
            self.gain_env -= 1.0 / (0.008 * sr);
            if self.gain_env <= 0.0 {
                self.state = State::Idle;
                self.gain_env = 0.0;
            }
        }
        self.level = self.level * 0.9995 + o.abs() * 0.0005;
        self.samples += 1;
        if self.samples > 2048 && self.level < 2e-5 {
            self.state = State::Idle;
        }
        o * self.gain_env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Estimate the fundamental via normalized autocorrelation with parabolic refinement.
    fn estimate_hz(x: &[f32], sr: f32) -> f32 {
        let min_lag = (sr / 1500.0) as usize;
        let max_lag = (sr / 30.0) as usize;
        let n = x.len() - max_lag;
        let ac = |lag: usize| -> f32 { (0..n).map(|i| x[i] * x[i + lag]).sum() };
        let vals: Vec<f32> = (0..=max_lag).map(|l| if l < min_lag { 0.0 } else { ac(l) }).collect();
        let peak = vals.iter().cloned().fold(0.0f32, f32::max);
        // first lag that reaches 90% of the global max avoids octave errors
        let mut best = min_lag;
        for l in min_lag + 1..max_lag {
            if vals[l] > 0.9 * peak && vals[l] >= vals[l - 1] && vals[l] >= vals[l + 1] {
                best = l;
                break;
            }
        }
        let (a, b, c) = (vals[best - 1], vals[best], vals[best + 1]);
        let shift = 0.5 * (a - c) / (a - 2.0 * b + c);
        sr / (best as f32 + shift)
    }

    fn render(pitch: f32, tone: ToneKind, curve: Vec<(u32, f32)>, secs: f32, skip: f32) -> Vec<f32> {
        let sr = 48000.0;
        let ev = NoteEvent { start: 0, end: 100_000, pitch, velocity: 0.8, curve, ..Default::default() };
        let mut v = StringVoice::new();
        let mut rng = Rng::new(1);
        v.pluck(0, &ev, tone, sr, &mut rng, 0);
        let ticks_per_sample = 960.0 * 2.0 / sr as f64; // 120 bpm
        let total = (secs * sr) as usize;
        let mut out = Vec::with_capacity(total);
        for i in 0..total {
            out.push(v.process(&ev, i as f64 * ticks_per_sample, sr));
        }
        out.split_off((skip * sr) as usize)
    }

    fn cents(a: f32, b: f32) -> f32 {
        1200.0 * (a / b).log2()
    }

    #[test]
    fn string_tuning_is_accurate() {
        for tone in [ToneKind::Clean, ToneKind::HighGain, ToneKind::Bass, ToneKind::Acoustic] {
            for pitch in [28.0f32, 40.0, 45.0, 52.0, 64.0, 76.0, 86.0] {
                if tone == ToneKind::Bass && pitch > 70.0 {
                    continue;
                }
                let y = render(pitch, tone, vec![], 0.5, 0.15);
                let hz = estimate_hz(&y, 48000.0);
                let want = midi_to_hz(pitch);
                let c = cents(hz, want);
                assert!(c.abs() < 5.0, "{tone:?} midi {pitch}: got {hz:.2} Hz want {want:.2} Hz ({c:+.1} cents)");
            }
        }
    }

    #[test]
    fn bend_reaches_target_pitch() {
        // full-step bend over the first quarter note (960 ticks = 0.5 s at 120 bpm), then held
        let y = render(59.0, ToneKind::Crunch, vec![(0, 0.0), (960, 2.0)], 1.2, 0.75);
        let hz = estimate_hz(&y, 48000.0);
        let c = cents(hz, midi_to_hz(61.0));
        assert!(c.abs() < 8.0, "bent pitch {hz:.2} Hz is {c:+.1} cents from target");
    }

    #[test]
    fn palm_mute_decays_faster() {
        let sr = 48000.0;
        let rms = |pm: bool| {
            let ev = NoteEvent { end: 100_000, pitch: 40.0, velocity: 0.8, palm_mute: pm, ..Default::default() };
            let mut v = StringVoice::new();
            let mut rng = Rng::new(1);
            v.pluck(0, &ev, ToneKind::HighGain, sr, &mut rng, 0);
            let y: Vec<f32> = (0..(0.6 * sr) as usize).map(|i| v.process(&ev, i as f64 * 0.04, sr)).collect();
            let tail = &y[(0.4 * sr) as usize..];
            (tail.iter().map(|s| s * s).sum::<f32>() / tail.len() as f32).sqrt()
        };
        let open = rms(false);
        let muted = rms(true);
        assert!(muted < open * 0.1, "muted tail {muted} vs open tail {open}");
    }
}
