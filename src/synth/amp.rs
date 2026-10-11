//! Per-track amplifier + cabinet simulation.

use super::dsp::{Biquad, Eq};
use super::nam::NamModel;
use crate::model::ToneKind;

/// One capture instance with its own filters and silence gate.
struct NamSide {
    model: NamModel,
    pre: Eq,
    post: Eq,
    tmp: Vec<f32>,
    /// Consecutive near-silent input samples, and the model's last output. Once the whole
    /// receptive field is silent the model's output is constant, so it can be skipped until
    /// the input returns (most of the time, for most tracks).
    quiet: usize,
    hold: f32,
    receptive: usize,
}

/// Pickup-signal level treated as silence (−120 dB), well under what the amp can reveal.
const SILENCE: f32 = 1e-6;

impl NamSide {
    /// Pickup signal in `buf` → amp output in place.
    fn process(&mut self, buf: &mut [f32], in_gain: f32, out_gain: f32) {
        let n = buf.len();
        self.tmp.resize(n, 0.0);
        let mut peak = 0.0f32;
        for x in buf.iter_mut() {
            *x = self.pre.process(*x);
            peak = peak.max(x.abs());
        }
        if peak < SILENCE && self.quiet >= self.receptive {
            self.tmp.fill(self.hold);
        } else {
            self.quiet = if peak < SILENCE { self.quiet + n } else { 0 };
            for x in buf.iter_mut() {
                *x *= in_gain;
            }
            self.model.process(buf, &mut self.tmp);
            self.hold = self.tmp[n - 1];
        }
        for (x, &y) in buf.iter_mut().zip(&self.tmp) {
            *x = self.post.process(y) * out_gain;
        }
    }
}

/// Neural Amp Modeler stage: the synthesized string (pickup signal) into one capture per
/// side. Drive mode adds an overdrive-style boost; clean mode turns the input down.
struct NamStage {
    /// One instance for a centred guitar, two (L/R) for a double-tracked one.
    sides: Vec<NamSide>,
    in_gain: f32,
    out_gain: f32,
}

impl NamStage {
    fn new(model: &NamModel, sr: f32, stereo: bool, clean: bool) -> Self {
        let db = |x: f32| 10f32.powf(x / 20.0);
        // The capture is a fixed amp setting. The clean tone rolls the guitar volume back to
        // the edge of breakup and voices it like a neck pickup; the high-gain amp's thin,
        // bright small-signal response is evened out after it.
        // Loaded pickup resonance (bridge humbucker / neck), which a real pickup puts on the DI.
        let (hp, pickup_hz, pickup_q) = if clean { (70.0, 2300.0, 0.8) } else { (110.0, 2600.0, 0.9) };
        let (low_db, lp) = if clean { (9.0, 2600.0) } else { (6.0, 6000.0) };
        let pre = || {
            let mut f = vec![Biquad::highpass(sr, hp, 0.6), Biquad::lowpass(sr, pickup_hz, pickup_q)];
            if !clean {
                // Tube-screamer-like tightening in front of the amp, as metal players do.
                f.push(Biquad::peaking(sr, 750.0, 0.9, 4.0));
            }
            Eq(f)
        };
        let post = || Eq(vec![Biquad::highpass(sr, 60.0, 0.7), Biquad::lowshelf(sr, 200.0, low_db), Biquad::lowpass(sr, lp, 0.7)]);
        // Captures are normalised to a reference output loudness; bring them to the
        // level of the other tones.
        let loud = model.loudness.unwrap_or(-23.0);
        let (in_gain, out_gain) = if clean { (db(-40.0), db(-13.0 - loud)) } else { (db(4.0), db(-23.0 - loud) * 3.0) };
        let side = || NamSide {
            model: model.clone(),
            pre: pre(),
            post: post(),
            tmp: Vec::new(),
            quiet: 0,
            hold: 0.0,
            receptive: model.receptive_field(),
        };
        NamStage { sides: (0..if stereo { 2 } else { 1 }).map(|_| side()).collect(), in_gain, out_gain }
    }

    #[cfg(test)]
    fn gated(&self) -> bool {
        self.sides[0].quiet >= self.sides[0].receptive
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        let (gi, go) = (self.in_gain, self.out_gain);
        match self.sides.as_mut_slice() {
            [mono] => {
                for (a, b) in l.iter_mut().zip(r.iter()) {
                    *a = 0.5 * (*a + b);
                }
                mono.process(l, gi, go);
                r.copy_from_slice(l);
            }
            // Double-tracked: the two captures are independent, so run them side by side.
            [left, right] => {
                rayon::join(|| left.process(l, gi, go), || right.process(r, gi, go));
            }
            _ => unreachable!(),
        }
    }
}

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
    /// Plain stereo gain stage (samples that already include amp/cab or acoustic drums).
    passthrough: bool,
    nam: Option<Box<NamStage>>,
}

impl AmpChain {
    /// `processed`: the source samples already contain the amplifier sound.
    pub fn new(tone: ToneKind, sr: f32, processed: bool) -> Self {
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
            passthrough: false,
            nam: None,
        };
        if processed {
            chain.passthrough = true;
            (chain.out_gain, chain.reverb_send) = match tone {
                ToneKind::Drums => (0.5, 0.06),
                _ => (0.42, 0.07),
            };
            return chain;
        }
        match tone {
            ToneKind::Nam => {
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
            ToneKind::Bass | ToneKind::FingerBass => {
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

    /// A guitar chain driven by a Neural Amp Modeler capture. `stereo`: separate amps for
    /// the left and right takes of a double-tracked part.
    pub fn with_nam(model: &NamModel, sr: f32, tone: ToneKind, stereo: bool) -> Self {
        let clean = tone == ToneKind::Clean;
        let mut chain = AmpChain::new(tone, sr, false);
        chain.nam = Some(Box::new(NamStage::new(model, sr, stereo && !clean, clean)));
        chain.reverb_send = if clean { 0.2 } else { 0.06 };
        chain
    }

    pub fn is_nam(&self) -> bool {
        self.nam.is_some()
    }

    /// Process a block in place.
    pub fn process_block(&mut self, l: &mut [f32], r: &mut [f32]) {
        if let Some(nam) = &mut self.nam {
            nam.process(l, r);
            return;
        }
        for (a, b) in l.iter_mut().zip(r.iter_mut()) {
            (*a, *b) = self.process_stereo(*a, *b);
        }
    }

    /// Stereo in/out; amp-simulated chains are mono internally.
    #[inline]
    pub fn process_stereo(&mut self, l: f32, r: f32) -> (f32, f32) {
        if self.passthrough {
            (l * self.out_gain, r * self.out_gain)
        } else {
            let y = self.process(0.5 * (l + r));
            (y, y)
        }
    }

    pub fn reset(&mut self) {
        self.pre.reset();
        self.post.reset();
        self.aa_up.reset();
        self.aa_down.reset();
        self.last_in = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The silence gate must not change the sound: compare against the same stage with
    /// gating disabled, across a note, a long rest (gated) and a second note.
    #[test]
    fn silence_gate_is_transparent() {
        let Some(path) = crate::synth::nam::find_model() else {
            eprintln!("skipping: no .nam model found");
            return;
        };
        let model = NamModel::load(&path).unwrap();
        for clean in [false, true] {
            let mut gated = NamStage::new(&model, 48000.0, false, clean);
            let mut always = NamStage::new(&model, 48000.0, false, clean);
            always.sides[0].receptive = usize::MAX;
            let note = |b: usize, i: usize| {
                let t = (b * 256 + i) as f32 / 48000.0;
                0.4 * (std::f32::consts::TAU * 110.0 * t).sin() * (-t * 8.0).exp()
            };
            let mut was_gated = false;
            for b in 0..400 {
                // Note in blocks 0..40 and 300..340, exact silence otherwise.
                let src: Vec<f32> = (0..256)
                    .map(|i| match b {
                        0..40 => note(b, i),
                        300..340 => note(b - 300, i),
                        _ => 0.0,
                    })
                    .collect();
                let (mut gl, mut gr, mut al, mut ar) = (src.clone(), src.clone(), src.clone(), src);
                gated.process(&mut gl, &mut gr);
                always.process(&mut al, &mut ar);
                was_gated |= gated.gated();
                for i in 0..256 {
                    assert!((gl[i] - al[i]).abs() < 1e-4, "clean={clean} block {b}: {} vs {}", gl[i], al[i]);
                }
            }
            assert!(was_gated, "clean={clean}: gate never engaged");
        }
    }
}
