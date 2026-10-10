//! Compact Freeverb-style stereo room reverb.

struct Comb {
    buf: Vec<f32>,
    i: usize,
    store: f32,
}

impl Comb {
    fn new(n: usize) -> Self {
        Comb { buf: vec![0.0; n], i: 0, store: 0.0 }
    }
    #[inline]
    fn process(&mut self, x: f32, feedback: f32, damp: f32) -> f32 {
        let y = self.buf[self.i];
        self.store = super::dsp::flush(y * (1.0 - damp) + self.store * damp);
        self.buf[self.i] = x + self.store * feedback;
        self.i = (self.i + 1) % self.buf.len();
        y
    }
}

struct Allpass {
    buf: Vec<f32>,
    i: usize,
}

impl Allpass {
    fn new(n: usize) -> Self {
        Allpass { buf: vec![0.0; n], i: 0 }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let b = self.buf[self.i];
        self.buf[self.i] = super::dsp::flush(x + b * 0.5);
        self.i = (self.i + 1) % self.buf.len();
        b - x
    }
}

pub struct Reverb {
    combs: [Vec<Comb>; 2],
    allpasses: [Vec<Allpass>; 2],
    feedback: f32,
    damp: f32,
}

impl Reverb {
    pub fn new(sr: f32) -> Self {
        let scale = sr / 44100.0;
        let comb_len = [1116, 1188, 1277, 1356, 1422, 1491];
        let ap_len = [556, 441, 341];
        let mk = |spread: usize| -> (Vec<Comb>, Vec<Allpass>) {
            (
                comb_len.iter().map(|&n| Comb::new(((n + spread) as f32 * scale * 0.7) as usize)).collect(),
                ap_len.iter().map(|&n| Allpass::new(((n + spread) as f32 * scale) as usize)).collect(),
            )
        };
        let (cl, al) = mk(0);
        let (cr, ar) = mk(23);
        Reverb { combs: [cl, cr], allpasses: [al, ar], feedback: 0.76, damp: 0.35 }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> (f32, f32) {
        let input = x * 0.05;
        let mut out = [0.0f32; 2];
        for ch in 0..2 {
            let mut acc = 0.0;
            for c in &mut self.combs[ch] {
                acc += c.process(input, self.feedback, self.damp);
            }
            for a in &mut self.allpasses[ch] {
                acc = a.process(acc);
            }
            out[ch] = acc;
        }
        (out[0], out[1])
    }
}
