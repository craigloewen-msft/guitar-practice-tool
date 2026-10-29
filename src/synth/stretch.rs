//! Pitch-preserving time stretch for the backing recording.
//!
//! A phase vocoder with identity phase locking (Laroche & Dolson). The whole recording is in
//! memory, so instead of buffering input it reads each analysis window straight from the
//! recording at the point the output will have reached when that window is heard. That keeps
//! the stretched audio in step with the transport with no added latency, and lets the rate change
//! at any moment.
//!
//! Every output hop needs two analysis windows one hop apart in the recording: the phase change
//! between them gives each partial's true frequency, which is then carried forward at the output
//! hop so pitch is unchanged whatever the rate. Bins near a spectral peak share that peak's phase
//! rotation, which keeps partials coherent and avoids the "phasey" sound of a plain vocoder. Both
//! channels are rotated by the same amount, worked out from their sum, so the stereo image holds.

use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// Window length in frames.
const N: usize = 4096;
/// Output hop in frames (8× overlap).
const HOP: usize = 512;
const BINS: usize = N / 2 + 1;
/// The transport moving further than this from where the stretch expected means a seek or loop.
const JUMP_S: f64 = 0.010;
/// Crossfade away from the old output after a jump.
const FADE_S: f32 = 0.006;

pub struct Stretcher {
    sr: f32,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    window: Vec<f32>,
    /// Recording read for one hop: N + HOP frames per channel.
    input: [Vec<f32>; 2],
    frame: Vec<f32>,
    scratch: Vec<Complex32>,
    /// Sum of both channels one hop earlier in the recording.
    prev_mid: Vec<Complex32>,
    spec: [Vec<Complex32>; 2],
    mid: Vec<Complex32>,
    /// The mid spectrum of the last frame synthesised, and the rotation each bin got.
    last_mid: Vec<Complex32>,
    lock: Vec<f32>,
    mag: Vec<f32>,
    peaks: Vec<usize>,
    /// Synthesis starts from the analysis phases (after a jump).
    fresh: bool,
    /// Overlap-add accumulator; its first HOP frames are complete after each synthesised frame.
    acc: [Vec<f32>; 2],
    ready: [Vec<f32>; 2],
    pos: usize,
    /// Position in the recording of the next output frame, in seconds. NaN while stopped.
    t: f64,
    /// Output abandoned at a jump, faded out under the new output.
    fade: [Vec<f32>; 2],
    fade_at: usize,
    fade_len: usize,
}

impl Stretcher {
    pub fn new(sr: f32) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(N);
        let inv = planner.plan_fft_inverse(N);
        let scratch_len = fwd.get_scratch_len().max(inv.get_scratch_len());
        let window = (0..N)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / N as f32).cos())
            .collect();
        let fade_len = ((sr * FADE_S) as usize).clamp(1, HOP + N);
        Stretcher {
            sr,
            fwd,
            inv,
            window,
            input: [vec![0.0; N + HOP], vec![0.0; N + HOP]],
            frame: vec![0.0; N],
            scratch: vec![Complex32::default(); scratch_len],
            prev_mid: vec![Complex32::default(); BINS],
            spec: [
                vec![Complex32::default(); BINS],
                vec![Complex32::default(); BINS],
            ],
            mid: vec![Complex32::default(); BINS],
            last_mid: vec![Complex32::default(); BINS],
            lock: vec![0.0; BINS],
            mag: vec![0.0; BINS],
            peaks: Vec::with_capacity(BINS),
            fresh: true,
            acc: [vec![0.0; N], vec![0.0; N]],
            ready: [vec![0.0; HOP], vec![0.0; HOP]],
            pos: HOP,
            t: f64::NAN,
            fade: [vec![0.0; HOP + N], vec![0.0; HOP + N]],
            fade_at: 0,
            fade_len,
        }
    }

    /// Forget the current output; the next frame starts afresh with no crossfade.
    pub fn stop(&mut self) {
        self.t = f64::NAN;
        self.fade_at = self.fade_len;
    }

    /// Produce one stereo output frame.
    ///
    /// `t` is where the transport says the recording is (None to carry on at `rate` on its own,
    /// e.g. while the transport is paused but the output is still fading out). `rate` is seconds
    /// of recording per second of output. `read(start, dt, left, right)` fills both slices with
    /// the recording sampled every `dt` seconds from `start`.
    pub fn next<F>(&mut self, t: Option<f64>, rate: f64, mut read: F) -> (f32, f32)
    where
        F: FnMut(f64, f64, &mut [f32], &mut [f32]),
    {
        let rate = rate.clamp(0.05, 4.0);
        match t {
            Some(t) if self.t.is_finite() && (t - self.t).abs() <= JUMP_S => self.t = t,
            Some(t) => self.restart(t, rate, &mut read),
            None if !self.t.is_finite() => return (0.0, 0.0),
            None => {}
        }
        if self.pos >= HOP {
            let tc = self.t + (N / 2) as f64 * rate / self.sr as f64;
            self.synth(tc, &mut read);
            self.advance(true);
        }
        let (mut l, mut r) = (self.ready[0][self.pos], self.ready[1][self.pos]);
        self.pos += 1;
        if self.fade_at < self.fade_len {
            let k = 1.0 - self.fade_at as f32 / self.fade_len as f32;
            l += (self.fade[0][self.fade_at] - l) * k;
            r += (self.fade[1][self.fade_at] - r) * k;
            self.fade_at += 1;
        }
        self.t += rate / self.sr as f64;
        (l, r)
    }

    /// Start again from `t`, synthesising the frames that overlap it so it is heard at once.
    fn restart<F>(&mut self, t: f64, rate: f64, read: &mut F)
    where
        F: FnMut(f64, f64, &mut [f32], &mut [f32]),
    {
        if self.t.is_finite() {
            // What would have been heard next: the rest of this hop, then the overlap-add tail.
            let rest = HOP - self.pos.min(HOP);
            for c in 0..2 {
                let fade = &mut self.fade[c];
                fade[..rest].copy_from_slice(&self.ready[c][self.pos.min(HOP)..]);
                fade[rest..rest + N].copy_from_slice(&self.acc[c]);
            }
            self.fade_at = 0;
        } else {
            self.fade_at = self.fade_len;
        }
        for a in &mut self.acc {
            a.fill(0.0);
        }
        self.t = t;
        self.fresh = true;
        let per_frame = rate / self.sr as f64;
        for m in (1..N / HOP).rev() {
            let tc = t + (N as f64 / 2.0 - (m * HOP) as f64) * per_frame;
            self.synth(tc, read);
            self.advance(false);
        }
        self.pos = HOP;
    }

    /// Move the first hop of the accumulator out (to `ready` if it is to be heard).
    fn advance(&mut self, keep: bool) {
        for c in 0..2 {
            if keep {
                self.ready[c].copy_from_slice(&self.acc[c][..HOP]);
            }
            self.acc[c].copy_within(HOP.., 0);
            self.acc[c][N - HOP..].fill(0.0);
        }
        self.pos = 0;
    }

    /// Synthesise the frame centred at recording time `tc` and add it to the accumulator.
    fn synth<F>(&mut self, tc: f64, read: &mut F)
    where
        F: FnMut(f64, f64, &mut [f32], &mut [f32]),
    {
        let dt = 1.0 / self.sr as f64;
        let [il, ir] = &mut self.input;
        read(tc - (N / 2 + HOP) as f64 * dt, dt, il, ir);

        // One hop earlier, mid only: that is all the phase advance needs.
        for (i, f) in self.frame.iter_mut().enumerate() {
            *f = (self.input[0][i] + self.input[1][i]) * self.window[i];
        }
        let _ =
            self.fwd
                .process_with_scratch(&mut self.frame, &mut self.prev_mid, &mut self.scratch);
        for c in 0..2 {
            for (i, f) in self.frame.iter_mut().enumerate() {
                *f = self.input[c][i + HOP] * self.window[i];
            }
            let _ = self.fwd.process_with_scratch(
                &mut self.frame,
                &mut self.spec[c],
                &mut self.scratch,
            );
        }
        for k in 0..BINS {
            self.mid[k] = self.spec[0][k] + self.spec[1][k];
            self.mag[k] = self.mid[k].norm_sqr();
        }

        self.peaks.clear();
        let m = &self.mag;
        for k in 2..BINS - 2 {
            if m[k] > m[k - 1] && m[k] >= m[k + 1] && m[k] > m[k - 2] && m[k] >= m[k + 2] {
                self.peaks.push(k);
            }
        }

        let fresh = std::mem::replace(&mut self.fresh, false);
        for (j, &p) in self.peaks.iter().enumerate() {
            let arg = self.mid[p].arg();
            let rot = if fresh {
                0.0
            } else {
                // Phase advance over one hop, unwrapped around what the bin's centre would give.
                let expected = TAU * p as f32 * HOP as f32 / N as f32;
                let advance = princarg(arg - self.prev_mid[p].arg() - expected) + expected;
                let last = self.last_mid[p].arg() + self.lock[p];
                (last + advance - arg).rem_euclid(TAU)
            };
            // The peak's region runs halfway to its neighbours.
            let lo = if j == 0 {
                1
            } else {
                (self.peaks[j - 1] + p) / 2 + 1
            };
            let hi = match self.peaks.get(j + 1) {
                Some(&q) => (p + q) / 2 + 1,
                None => BINS - 1,
            };
            let phasor = Complex32::from_polar(1.0, rot);
            for k in lo..hi {
                self.lock[k] = rot;
                self.spec[0][k] *= phasor;
                self.spec[1][k] *= phasor;
            }
        }
        if self.peaks.is_empty() {
            self.lock.fill(0.0);
        }
        self.last_mid.copy_from_slice(&self.mid);

        // Hann on analysis and synthesis at 8× overlap sums to 3; the inverse FFT scales by N.
        let norm = 1.0 / (3.0 * N as f32);
        for c in 0..2 {
            let _ = self.inv.process_with_scratch(
                &mut self.spec[c],
                &mut self.frame,
                &mut self.scratch,
            );
            for (i, a) in self.acc[c].iter_mut().enumerate() {
                *a += self.frame[i] * self.window[i] * norm;
            }
        }
    }
}

fn princarg(x: f32) -> f32 {
    x - TAU * ((x + PI) / TAU).floor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth::string::tests::estimate_hz;

    const SR: f32 = 48_000.0;

    /// Render `secs` of output at `rate`, reading `f(t)` (identical on both channels).
    fn render(f: impl Fn(f64) -> f32, rate: f64, secs: f32) -> Vec<f32> {
        let mut s = Stretcher::new(SR);
        let mut t = 0.5;
        (0..(secs * SR) as usize)
            .map(|_| {
                let (l, r) = s.next(Some(t), rate, |start, dt, ol, or| {
                    for i in 0..ol.len() {
                        ol[i] = f(start + i as f64 * dt);
                        or[i] = ol[i];
                    }
                });
                assert!((l - r).abs() < 1e-6);
                t += rate / SR as f64;
                l
            })
            .collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn slowing_down_keeps_pitch_and_level() {
        let tone = |t: f64| 0.5 * (std::f64::consts::TAU * 220.0 * t).sin() as f32;
        for rate in [0.5, 0.75, 1.25] {
            let out = render(tone, rate, 1.0);
            let hz = estimate_hz(&out[4800..], SR);
            assert!((hz - 220.0).abs() < 1.0, "rate {rate}: {hz} Hz");
            let level = rms(&out[4800..]);
            assert!(
                (level - 0.5 / 2f32.sqrt()).abs() < 0.02,
                "rate {rate}: rms {level}"
            );
        }
    }

    #[test]
    fn rate_one_passes_audio_through() {
        let sig = |t: f64| {
            let tau = std::f64::consts::TAU;
            (0.3 * (tau * 196.0 * t).sin() + 0.2 * (tau * 523.0 * t).sin() * (tau * 3.0 * t).cos())
                as f32
        };
        let out = render(sig, 1.0, 0.5);
        for (i, &v) in out.iter().enumerate() {
            let want = sig(0.5 + i as f64 / SR as f64);
            assert!((v - want).abs() < 1e-3, "frame {i}: {v} vs {want}");
        }
    }

    #[test]
    fn follows_the_recording_in_time() {
        // A tone that changes pitch at 1s into the recording, played at half speed from 0.5s:
        // the change should be heard 1s into the output, not before.
        let sweep = |t: f64| {
            let hz = if t < 1.0 { 220.0 } else { 330.0 };
            0.5 * (std::f64::consts::TAU * hz * t).sin() as f32
        };
        let out = render(sweep, 0.5, 1.6);
        let at = |s: f32| estimate_hz(&out[(s * SR) as usize..((s + 0.2) * SR) as usize], SR);
        assert!((at(0.7) - 220.0).abs() < 2.0, "{}", at(0.7));
        assert!((at(1.2) - 330.0).abs() < 2.0, "{}", at(1.2));
    }

    #[test]
    fn a_jump_restarts_cleanly() {
        let tone = |t: f64| 0.5 * (std::f64::consts::TAU * 220.0 * t).sin() as f32;
        let mut s = Stretcher::new(SR);
        let mut t = 2.0;
        let mut out = Vec::new();
        for i in 0..(SR as usize) {
            if i == SR as usize / 2 {
                t = 0.25;
            }
            let (l, _) = s.next(Some(t), 0.5, |start, dt, ol, or| {
                for i in 0..ol.len() {
                    ol[i] = tone(start + i as f64 * dt);
                    or[i] = ol[i];
                }
            });
            out.push(l);
            t += 0.5 / SR as f64;
        }
        // Full level straight away after the jump, and no sample far out of range.
        assert!(out.iter().all(|v| v.abs() < 0.6));
        let after = &out[SR as usize / 2 + 480..SR as usize / 2 + 4800];
        assert!(
            (rms(after) - 0.5 / 2f32.sqrt()).abs() < 0.03,
            "{}",
            rms(after)
        );
    }
}
