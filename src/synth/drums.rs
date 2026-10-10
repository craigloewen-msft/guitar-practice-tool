//! Synthesized drum kit addressed by General MIDI percussion keys.

use super::dsp::{Biquad, Rng};
use std::f32::consts::TAU;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Kick,
    Snare,
    SideStick,
    Clap,
    ClosedHat,
    PedalHat,
    OpenHat,
    Tom(u8),
    Crash,
    China,
    Splash,
    Ride,
    RideBell,
    Cowbell,
    Tambourine,
    Click,
}

fn kind_for(key: i32) -> Kind {
    match key {
        35 | 36 => Kind::Kick,
        37 => Kind::SideStick,
        38 | 40 => Kind::Snare,
        39 => Kind::Clap,
        42 => Kind::ClosedHat,
        44 => Kind::PedalHat,
        46 => Kind::OpenHat,
        41 | 43 | 45 | 47 | 48 | 50 => Kind::Tom(key as u8),
        49 | 57 => Kind::Crash,
        52 => Kind::China,
        55 => Kind::Splash,
        51 | 59 => Kind::Ride,
        53 => Kind::RideBell,
        56 => Kind::Cowbell,
        54 => Kind::Tambourine,
        _ => Kind::Click,
    }
}

/// Inharmonic square-wave bank (classic analog cymbal/hat source).
const METAL: [f32; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

pub struct DrumVoice {
    pub active: bool,
    pub track: usize,
    kind: Kind,
    t: f32,
    vel: f32,
    phase: [f32; 6],
    tone_phase: f32,
    f_start: f32,
    f_end: f32,
    tone_decay: f32,
    noise_decay: f32,
    tone_amp: f32,
    noise_amp: f32,
    metal_amp: f32,
    metal_scale: f32,
    filt1: Biquad,
    filt2: Biquad,
    filt3: Biquad,
    choke: f32,
    pub age: u64,
}

impl DrumVoice {
    pub fn new() -> Self {
        DrumVoice {
            active: false,
            track: 0,
            kind: Kind::Click,
            t: 0.0,
            vel: 0.0,
            phase: [0.0; 6],
            tone_phase: 0.0,
            f_start: 0.0,
            f_end: 0.0,
            tone_decay: 0.1,
            noise_decay: 0.1,
            tone_amp: 0.0,
            noise_amp: 0.0,
            metal_amp: 0.0,
            metal_scale: 1.0,
            filt1: Biquad::default(),
            filt2: Biquad::default(),
            filt3: Biquad::default(),
            choke: 1.0,
            age: 0,
        }
    }

    pub fn is_hat(&self) -> bool {
        matches!(self.kind, Kind::OpenHat | Kind::ClosedHat | Kind::PedalHat)
    }

    pub fn trigger(&mut self, track: usize, key: i32, vel: f32, sr: f32, rng: &mut Rng, age: u64) -> bool {
        let kind = kind_for(key);
        *self = DrumVoice::new();
        self.active = true;
        self.track = track;
        self.kind = kind;
        self.vel = vel;
        self.age = age;
        for p in &mut self.phase {
            *p = rng.next().abs();
        }
        match kind {
            Kind::Kick => {
                self.f_start = 160.0;
                self.f_end = 48.0;
                self.tone_decay = 0.32;
                self.tone_amp = 1.3;
                self.noise_decay = 0.006;
                self.noise_amp = 0.5;
                self.filt1 = Biquad::lowpass(sr, 3500.0, 0.7);
            }
            Kind::Snare => {
                self.f_start = 260.0;
                self.f_end = 185.0;
                self.tone_decay = 0.07;
                self.tone_amp = 0.7;
                self.noise_decay = 0.16;
                self.noise_amp = 0.85;
                self.filt1 = Biquad::highpass(sr, 1200.0, 0.7);
                self.filt2 = Biquad::peaking(sr, 4500.0, 1.0, 4.0);
                self.filt3 = Biquad::lowpass(sr, 10000.0, 0.7);
            }
            Kind::SideStick => {
                self.f_start = 1700.0;
                self.f_end = 1500.0;
                self.tone_decay = 0.02;
                self.tone_amp = 0.5;
                self.noise_decay = 0.015;
                self.noise_amp = 0.4;
                self.filt1 = Biquad::bandpass(sr, 3000.0, 1.0);
            }
            Kind::Clap => {
                self.noise_decay = 0.12;
                self.noise_amp = 0.9;
                self.filt1 = Biquad::bandpass(sr, 1200.0, 1.2);
            }
            Kind::ClosedHat | Kind::PedalHat => {
                self.metal_amp = 0.5;
                self.noise_amp = 0.25;
                self.noise_decay = if kind == Kind::PedalHat { 0.03 } else { 0.045 };
                self.tone_decay = self.noise_decay;
                self.metal_scale = 1.0;
                self.filt1 = Biquad::highpass(sr, 7000.0, 0.7);
                self.filt2 = Biquad::bandpass(sr, 10000.0, 0.9);
            }
            Kind::OpenHat => {
                self.metal_amp = 0.5;
                self.noise_amp = 0.25;
                self.noise_decay = 0.35;
                self.tone_decay = 0.35;
                self.metal_scale = 1.0;
                self.filt1 = Biquad::highpass(sr, 6500.0, 0.7);
                self.filt2 = Biquad::bandpass(sr, 9500.0, 0.8);
            }
            Kind::Tom(k) => {
                let f = match k {
                    41 => 82.0,
                    43 => 98.0,
                    45 => 115.0,
                    47 => 135.0,
                    48 => 155.0,
                    _ => 180.0,
                };
                self.f_start = f * 1.6;
                self.f_end = f;
                self.tone_decay = 0.3;
                self.tone_amp = 1.0;
                self.noise_decay = 0.03;
                self.noise_amp = 0.3;
                self.filt1 = Biquad::lowpass(sr, 4000.0, 0.7);
            }
            Kind::Crash | Kind::China | Kind::Splash => {
                self.metal_amp = 0.35;
                self.noise_amp = 0.45;
                self.noise_decay = match kind {
                    Kind::Splash => 0.5,
                    Kind::China => 1.1,
                    _ => 1.4,
                };
                self.tone_decay = self.noise_decay;
                self.metal_scale = match kind {
                    Kind::China => 0.8,
                    Kind::Splash => 1.4,
                    _ => 1.1,
                };
                self.filt1 = Biquad::highpass(sr, if kind == Kind::China { 2500.0 } else { 4000.0 }, 0.7);
                self.filt2 = Biquad::peaking(sr, 7000.0, 0.7, 3.0);
                self.filt3 = Biquad::lowpass(sr, 14000.0, 0.7);
            }
            Kind::Ride | Kind::RideBell => {
                self.metal_amp = 0.35;
                self.noise_amp = 0.12;
                self.noise_decay = 0.9;
                self.tone_decay = 0.9;
                self.metal_scale = 1.5;
                self.f_start = if kind == Kind::RideBell { 2400.0 } else { 3100.0 };
                self.f_end = self.f_start;
                self.tone_amp = if kind == Kind::RideBell { 0.5 } else { 0.15 };
                self.filt1 = Biquad::highpass(sr, 3500.0, 0.7);
                self.filt2 = Biquad::bandpass(sr, 8000.0, 0.6);
            }
            Kind::Cowbell => {
                self.f_start = 560.0;
                self.f_end = 560.0;
                self.tone_decay = 0.12;
                self.tone_amp = 0.6;
                self.filt1 = Biquad::bandpass(sr, 1000.0, 2.0);
            }
            Kind::Tambourine => {
                self.metal_amp = 0.3;
                self.noise_amp = 0.4;
                self.noise_decay = 0.12;
                self.tone_decay = 0.12;
                self.metal_scale = 2.5;
                self.filt1 = Biquad::highpass(sr, 6000.0, 0.7);
            }
            Kind::Click => {
                self.noise_decay = 0.02;
                self.noise_amp = 0.4;
                self.filt1 = Biquad::bandpass(sr, 2500.0, 1.0);
            }
        }
        self.is_hat()
    }

    /// Close an open hi-hat (when a closed/pedal hat is played).
    pub fn choke(&mut self) {
        if self.kind == Kind::OpenHat {
            self.choke = 0.999;
        }
    }

    #[inline]
    pub fn process(&mut self, sr: f32, rng: &mut Rng) -> f32 {
        if !self.active {
            return 0.0;
        }
        let dt = 1.0 / sr;
        let t = self.t;
        self.t += dt;
        let mut tone = 0.0;
        if self.tone_amp > 0.0 {
            let f = self.f_end + (self.f_start - self.f_end) * (-t / 0.035).exp();
            self.tone_phase = (self.tone_phase + f * dt).fract();
            tone = (self.tone_phase * TAU).sin() * (-t / self.tone_decay).exp() * self.tone_amp;
            if matches!(self.kind, Kind::Cowbell) {
                tone = (tone * 3.0).tanh() * 0.5;
            }
        }
        let mut noise = 0.0;
        if self.noise_amp > 0.0 {
            let env = if self.kind == Kind::Clap {
                // Multi-burst clap envelope.
                let burst = if t < 0.03 { (-(t % 0.01) / 0.003).exp() } else { 0.0 };
                burst.max((-(t - 0.02).max(0.0) / self.noise_decay).exp() * if t >= 0.02 { 1.0 } else { 0.0 })
            } else {
                (-t / self.noise_decay).exp()
            };
            noise = rng.next() * env * self.noise_amp;
        }
        let mut metal = 0.0;
        if self.metal_amp > 0.0 {
            for (i, p) in self.phase.iter_mut().enumerate() {
                *p = (*p + METAL[i] * self.metal_scale * dt).fract();
                metal += if *p < 0.5 { 1.0 } else { -1.0 };
            }
            metal *= self.metal_amp / 6.0 * (-t / self.tone_decay.max(0.01)).exp();
        }
        let mut y = match self.kind {
            Kind::Kick | Kind::Tom(_) => tone + self.filt1.process(noise),
            Kind::Snare => tone + self.filt3.process(self.filt2.process(self.filt1.process(noise))),
            _ => self.filt3.process(self.filt2.process(self.filt1.process(metal + noise))) + tone,
        };
        if self.choke < 1.0 {
            self.choke = (self.choke - dt / 0.01).max(0.0);
            y *= self.choke;
        }
        // Attack de-click.
        y *= (t / 0.0005).min(1.0) * self.vel;
        let longest = self.tone_decay.max(self.noise_decay).max(0.02);
        if t > longest * 7.0 || self.choke <= 0.0 {
            self.active = false;
        }
        y
    }
}
