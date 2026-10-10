//! Small DSP building blocks.

use std::f32::consts::PI;

#[derive(Clone)]
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Rng(seed.max(1))
    }
    /// Uniform in [-1, 1).
    #[inline]
    pub fn next(&mut self) -> f32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    /// Identity (pass-through) filter.
    fn default() -> Self {
        Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, z1: 0.0, z2: 0.0 }
    }
}

impl Biquad {
    fn from_raw(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Self {
        Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z1: 0.0, z2: 0.0 }
    }

    fn w0(fs: f32, f: f32) -> (f32, f32) {
        let w = 2.0 * PI * (f / fs).min(0.49);
        (w.cos(), w.sin())
    }

    pub fn lowpass(fs: f32, f: f32, q: f32) -> Self {
        let (c, s) = Self::w0(fs, f);
        let al = s / (2.0 * q);
        Self::from_raw((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn highpass(fs: f32, f: f32, q: f32) -> Self {
        let (c, s) = Self::w0(fs, f);
        let al = s / (2.0 * q);
        Self::from_raw((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn bandpass(fs: f32, f: f32, q: f32) -> Self {
        let (c, s) = Self::w0(fs, f);
        let al = s / (2.0 * q);
        Self::from_raw(al, 0.0, -al, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn peaking(fs: f32, f: f32, q: f32, db: f32) -> Self {
        let (c, s) = Self::w0(fs, f);
        let a = 10f32.powf(db / 40.0);
        let al = s / (2.0 * q);
        Self::from_raw(1.0 + al * a, -2.0 * c, 1.0 - al * a, 1.0 + al / a, -2.0 * c, 1.0 - al / a)
    }

    pub fn lowshelf(fs: f32, f: f32, db: f32) -> Self {
        let (c, s) = Self::w0(fs, f);
        let a = 10f32.powf(db / 40.0);
        let al = s / 2.0 * 2f32.sqrt();
        let sa = 2.0 * a.sqrt() * al;
        Self::from_raw(
            a * ((a + 1.0) - (a - 1.0) * c + sa),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - sa),
            (a + 1.0) + (a - 1.0) * c + sa,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - sa,
        )
    }

    pub fn highshelf(fs: f32, f: f32, db: f32) -> Self {
        let (c, s) = Self::w0(fs, f);
        let a = 10f32.powf(db / 40.0);
        let al = s / 2.0 * 2f32.sqrt();
        let sa = 2.0 * a.sqrt() * al;
        Self::from_raw(
            a * ((a + 1.0) + (a - 1.0) * c + sa),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - sa),
            (a + 1.0) - (a - 1.0) * c + sa,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - sa,
        )
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = flush(self.b1 * x - self.a1 * y + self.z2);
        self.z2 = flush(self.b2 * x - self.a2 * y);
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }
}

/// Chain of biquads.
#[derive(Clone, Default)]
pub struct Eq(pub Vec<Biquad>);

impl Eq {
    #[inline]
    pub fn process(&mut self, mut x: f32) -> f32 {
        for b in &mut self.0 {
            x = b.process(x);
        }
        x
    }
    pub fn reset(&mut self) {
        for b in &mut self.0 {
            b.reset();
        }
    }
}

#[inline]
pub fn flush(x: f32) -> f32 {
    if x.abs() < 1e-20 { 0.0 } else { x }
}

#[inline]
pub fn midi_to_hz(p: f32) -> f32 {
    440.0 * ((p - 69.0) / 12.0).exp2()
}

/// Smooth saturating clipper used as a master limiter.
#[inline]
pub fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a < 0.8 {
        x
    } else {
        let over = a - 0.8;
        x.signum() * (0.8 + 0.2 * (over / 0.2).tanh())
    }
}
