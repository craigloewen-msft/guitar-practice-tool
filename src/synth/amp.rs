//! Per-track amplifier + cabinet simulation.

use super::dsp::{Biquad, Eq};
use crate::model::ToneKind;

pub struct AmpChain {
    pre: Eq,
    gain: f32,
    bias: f32,
    drive: bool,
    /// Anti-aliasing filters for the 4x oversampled clipper.
    aa_up: Eq,
    aa_down: Eq,
    last_in: f32,
    post: Eq,
    out_gain: f32,
    pub reverb_send: f32,
}

impl AmpChain {
    pub fn new(tone: ToneKind, sr: f32) -> Self {
        let os = sr * 4.0;
        let aa = || Eq(vec![Biquad::lowpass(os, sr * 0.42, 0.54), Biquad::lowpass(os, sr * 0.42, 1.31)]);
        let mut chain = AmpChain {
            pre: Eq::default(),
            gain: 1.0,
            bias: 0.0,
            drive: false,
            aa_up: aa(),
            aa_down: aa(),
            last_in: 0.0,
            post: Eq::default(),
            out_gain: 1.0,
            reverb_send: 0.1,
        };
        match tone {
            ToneKind::HighGain => {
                chain.pre = Eq(vec![
                    Biquad::highpass(sr, 130.0, 0.7),
                    Biquad::peaking(sr, 750.0, 0.8, 7.0),
                    Biquad::lowpass(sr, 7000.0, 0.7),
                ]);
                chain.gain = 40.0;
                chain.bias = 0.15;
                chain.drive = true;
                chain.post = Eq(vec![
                    Biquad::highpass(sr, 75.0, 0.7),
                    Biquad::peaking(sr, 110.0, 1.0, 3.5),
                    Biquad::peaking(sr, 480.0, 0.9, -6.0),
                    Biquad::peaking(sr, 2400.0, 1.2, 4.0),
                    Biquad::lowpass(sr, 4800.0, 1.1),
                    Biquad::lowpass(sr, 6500.0, 0.6),
                ]);
                chain.out_gain = 0.32;
                chain.reverb_send = 0.08;
            }
            ToneKind::Crunch => {
                chain.pre = Eq(vec![
                    Biquad::highpass(sr, 100.0, 0.7),
                    Biquad::peaking(sr, 900.0, 0.8, 5.0),
                    Biquad::lowpass(sr, 7500.0, 0.7),
                ]);
                chain.gain = 11.0;
                chain.bias = 0.2;
                chain.drive = true;
                chain.post = Eq(vec![
                    Biquad::highpass(sr, 80.0, 0.7),
                    Biquad::peaking(sr, 600.0, 0.9, -2.0),
                    Biquad::peaking(sr, 2200.0, 1.0, 3.5),
                    Biquad::lowpass(sr, 5200.0, 1.0),
                    Biquad::lowpass(sr, 7000.0, 0.6),
                ]);
                chain.out_gain = 0.42;
                chain.reverb_send = 0.12;
            }
            ToneKind::Clean => {
                chain.pre = Eq(vec![Biquad::highpass(sr, 70.0, 0.7)]);
                chain.gain = 0.9;
                chain.bias = 0.05;
                chain.drive = true;
                chain.post = Eq(vec![
                    Biquad::peaking(sr, 300.0, 0.8, -2.0),
                    Biquad::peaking(sr, 3000.0, 0.8, 2.5),
                    Biquad::lowpass(sr, 7500.0, 0.8),
                ]);
                chain.out_gain = 0.55;
                chain.reverb_send = 0.25;
            }
            ToneKind::Bass => {
                chain.pre = Eq(vec![Biquad::highpass(sr, 32.0, 0.7), Biquad::lowshelf(sr, 120.0, 3.0)]);
                chain.gain = 1.6;
                chain.bias = 0.05;
                chain.drive = true;
                chain.post = Eq(vec![
                    Biquad::peaking(sr, 800.0, 1.0, 2.0),
                    Biquad::lowpass(sr, 3500.0, 0.7),
                ]);
                chain.out_gain = 0.55;
                chain.reverb_send = 0.03;
            }
            ToneKind::Acoustic => {
                chain.pre = Eq(vec![
                    Biquad::highpass(sr, 70.0, 0.7),
                    Biquad::peaking(sr, 105.0, 2.5, 5.0),
                    Biquad::peaking(sr, 230.0, 2.0, 3.0),
                    Biquad::highshelf(sr, 5000.0, 3.0),
                ]);
                chain.out_gain = 0.6;
                chain.reverb_send = 0.2;
            }
            ToneKind::Drums => {
                chain.out_gain = 0.38;
                chain.reverb_send = 0.07;
            }
        }
        chain
    }

    #[inline]
    fn clip(&self, x: f32) -> f32 {
        (x + self.bias).tanh() - self.bias.tanh()
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = self.pre.process(x);
        let y = if self.drive {
            let xg = x * self.gain;
            let mut out = 0.0;
            // 4x oversampled clipper.
            for k in 1..=4 {
                let t = k as f32 * 0.25;
                let up = self.aa_up.process(self.last_in + (xg - self.last_in) * t);
                out = self.aa_down.process(self.clip(up));
            }
            self.last_in = xg;
            out
        } else {
            x
        };
        self.post.process(y) * self.out_gain
    }

    pub fn reset(&mut self) {
        self.pre.reset();
        self.post.reset();
        self.aa_up.reset();
        self.aa_down.reset();
        self.last_in = 0.0;
    }
}
