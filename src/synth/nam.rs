//! Neural Amp Modeler (NAM) inference: a pure-Rust port of the WaveNet
//! architecture from NeuralAmpModelerCore (https://github.com/sdatkinson/NeuralAmpModelerCore).
//!
//! Supports classic "A1" WaveNet captures (gated or not, 1x1 heads) and the
//! newer "A2" exports (per-layer kernel sizes, bottleneck + layer1x1, causal
//! head convolutions, `SlimmableContainer` wrappers).  Weight ordering and the
//! layer math follow `NAM/wavenet/model.cpp` and `NAM/conv1d.cpp` exactly, so
//! output matches the reference `render` tool to float precision.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;

/// Frames processed per internal block; larger callers are chunked.
const BLOCK: usize = 64;
/// Extra room in each history buffer so the shift-down only happens rarely.
const HIST_SLACK: usize = 4096;

#[derive(Clone, Copy, Debug)]
enum Act {
    Tanh,
    Hardtanh,
    Fasttanh,
    Relu,
    LeakyRelu(f32),
    Sigmoid,
    Silu,
    Hardswish,
    Softsign,
    LeakyHardtanh { min_val: f32, max_val: f32, min_slope: f32, max_slope: f32 },
}

impl Act {
    fn parse(v: &Value) -> Result<Act> {
        let (name, obj) = match v {
            Value::Null => return Ok(Act::Sigmoid),
            Value::String(s) => (s.as_str(), None),
            Value::Object(o) => (
                o.get("type").and_then(Value::as_str).ok_or_else(|| anyhow!("activation without type"))?,
                Some(o),
            ),
            _ => bail!("bad activation {v}"),
        };
        let f = |k: &str, d: f32| obj.and_then(|o| o.get(k)).and_then(Value::as_f64).map_or(d, |x| x as f32);
        Ok(match name {
            "Tanh" => Act::Tanh,
            "Hardtanh" => Act::Hardtanh,
            "Fasttanh" => Act::Fasttanh,
            "ReLU" => Act::Relu,
            "LeakyReLU" => Act::LeakyRelu(f("negative_slope", 0.01)),
            "PReLU" => {
                if obj.and_then(|o| o.get("negative_slopes")).is_some() {
                    bail!("per-channel PReLU is not supported");
                }
                Act::LeakyRelu(f("negative_slope", 0.01))
            }
            "Sigmoid" => Act::Sigmoid,
            "SiLU" => Act::Silu,
            "Hardswish" => Act::Hardswish,
            "Softsign" => Act::Softsign,
            "LeakyHardtanh" | "LeakyHardTanh" => Act::LeakyHardtanh {
                min_val: f("min_val", -1.0),
                max_val: f("max_val", 1.0),
                min_slope: f("min_slope", 0.01),
                max_slope: f("max_slope", 0.01),
            },
            other => bail!("unsupported activation {other}"),
        })
    }

    #[inline(always)]
    fn apply(self, x: f32) -> f32 {
        match self {
            Act::Tanh => x.tanh(),
            Act::Hardtanh => x.clamp(-1.0, 1.0),
            Act::Fasttanh => {
                let ax = x.abs();
                let x2 = x * x;
                (x * (2.455_507_5 + 2.455_507_5 * ax + (0.893_229_85 + 0.821_226_7 * ax) * x2))
                    / (2.445_066_3 + (2.445_066_3 + x2) * (x + 0.814_642_7 * x * ax).abs())
            }
            Act::Relu => x.max(0.0),
            Act::LeakyRelu(ns) => {
                if x > 0.0 {
                    x
                } else {
                    ns * x
                }
            }
            Act::Sigmoid => 1.0 / (1.0 + (-x).exp()),
            Act::Silu => x / (1.0 + (-x).exp()),
            Act::Hardswish => x * (x + 3.0).clamp(0.0, 6.0) * (1.0 / 6.0),
            Act::Softsign => x / (1.0 + x.abs()),
            Act::LeakyHardtanh { min_val, max_val, min_slope, max_slope } => {
                if x < min_val {
                    (x - min_val) * min_slope + min_val
                } else if x > max_val {
                    (x - max_val) * max_slope + max_val
                } else {
                    x
                }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Gating {
    None,
    Gated,
    Blended,
}

/// Sequential reader over the flat weight vector.
struct Weights<'a> {
    w: &'a [f32],
    i: usize,
}

impl Weights<'_> {
    fn take(&mut self) -> Result<f32> {
        let v = *self.w.get(self.i).ok_or_else(|| anyhow!("model has too few weights"))?;
        self.i += 1;
        Ok(v)
    }
}

/// Causal dilated 1-D convolution.  Weights are stored `[k][out][in]`.
#[derive(Clone)]
struct Conv {
    cin: usize,
    cout: usize,
    k: usize,
    d: usize,
    w: Vec<f32>,
    bias: Vec<f32>,
}

impl Conv {
    fn new(cin: usize, cout: usize, k: usize, d: usize, groups: usize, bias: bool, ws: &mut Weights) -> Result<Conv> {
        if groups == 0 || cin % groups != 0 || cout % groups != 0 {
            bail!("bad conv groups {groups} for {cin}->{cout}");
        }
        let mut w = vec![0.0; k * cout * cin];
        let (og, ig) = (cout / groups, cin / groups);
        // NAM flattening order: group, out, in, kernel tap.
        for g in 0..groups {
            for o in 0..og {
                for i in 0..ig {
                    for kk in 0..k {
                        w[(kk * cout + g * og + o) * cin + g * ig + i] = ws.take()?;
                    }
                }
            }
        }
        let bias = if bias { (0..cout).map(|_| ws.take()).collect::<Result<_>>()? } else { vec![0.0; cout] };
        Ok(Conv { cin, cout, k, d, w, bias })
    }

    fn lookback(&self) -> usize {
        self.d * (self.k - 1)
    }

    /// `hist` holds the input frames; frame `t` of the current block lives at
    /// `hist.frame(t)`.  Writes `n` frames to `out` (frame-major).
    fn run(&self, hist: &Hist, n: usize, out: &mut [f32]) {
        match (self.cin, self.cout) {
            (8, 8) => self.run_fixed::<8, 8>(hist, n, out),
            (8, 1) => self.run_fixed::<8, 1>(hist, n, out),
            (16, 16) => self.run_fixed::<16, 16>(hist, n, out),
            (16, 1) => self.run_fixed::<16, 1>(hist, n, out),
            (3, 3) => self.run_fixed::<3, 3>(hist, n, out),
            (3, 1) => self.run_fixed::<3, 1>(hist, n, out),
            _ => self.run_generic(hist, n, out),
        }
    }

    /// Same as [`Self::run_generic`] with compile-time channel counts so the
    /// inner loops fully unroll and vectorise.
    fn run_fixed<const CI: usize, const CO: usize>(&self, hist: &Hist, n: usize, out: &mut [f32]) {
        let bias: &[f32; CO] = self.bias[..CO].try_into().unwrap();
        let rows: &[[f32; CI]] = self.w.as_chunks::<CI>().0;
        let (k, d) = (self.k, self.d);
        for t in 0..n {
            let mut acc = *bias;
            for kk in 0..k {
                let x: &[f32; CI] = hist.frame(t, d * (k - 1 - kk)).try_into().unwrap();
                let wk = &rows[kk * CO..(kk + 1) * CO];
                for oc in 0..CO {
                    let wr = &wk[oc];
                    let mut s = 0.0;
                    for ic in 0..CI {
                        s += wr[ic] * x[ic];
                    }
                    acc[oc] += s;
                }
            }
            out[t * CO..(t + 1) * CO].copy_from_slice(&acc);
        }
    }

    fn run_generic(&self, hist: &Hist, n: usize, out: &mut [f32]) {
        let (cin, cout) = (self.cin, self.cout);
        for t in 0..n {
            let o = &mut out[t * cout..(t + 1) * cout];
            o.copy_from_slice(&self.bias);
            for kk in 0..self.k {
                let x = hist.frame(t, self.d * (self.k - 1 - kk));
                let wk = &self.w[kk * cout * cin..(kk + 1) * cout * cin];
                for (oc, ov) in o.iter_mut().enumerate() {
                    let wr = &wk[oc * cin..(oc + 1) * cin];
                    let mut acc = 0.0;
                    for ic in 0..cin {
                        acc += wr[ic] * x[ic];
                    }
                    *ov += acc;
                }
            }
        }
    }

    /// Pointwise (k = 1) version that reads straight from a frame-major slice.
    fn run_1x1(&self, input: &[f32], in_stride: usize, n: usize, out: &mut [f32]) {
        match (self.cin, self.cout) {
            (1, 8) => self.run_1x1_fixed::<1, 8>(input, in_stride, n, out),
            (8, 8) => self.run_1x1_fixed::<8, 8>(input, in_stride, n, out),
            (1, 16) => self.run_1x1_fixed::<1, 16>(input, in_stride, n, out),
            (16, 16) => self.run_1x1_fixed::<16, 16>(input, in_stride, n, out),
            (1, 3) => self.run_1x1_fixed::<1, 3>(input, in_stride, n, out),
            (3, 3) => self.run_1x1_fixed::<3, 3>(input, in_stride, n, out),
            _ => self.run_1x1_generic(input, in_stride, n, out),
        }
    }

    fn run_1x1_fixed<const CI: usize, const CO: usize>(&self, input: &[f32], in_stride: usize, n: usize, out: &mut [f32]) {
        let rows: &[[f32; CI]] = self.w.as_chunks::<CI>().0;
        let bias: &[f32; CO] = self.bias[..CO].try_into().unwrap();
        for t in 0..n {
            let x: &[f32; CI] = input[t * in_stride..t * in_stride + CI].try_into().unwrap();
            let mut acc = *bias;
            for oc in 0..CO {
                let mut s = 0.0;
                for ic in 0..CI {
                    s += rows[oc][ic] * x[ic];
                }
                acc[oc] += s;
            }
            out[t * CO..(t + 1) * CO].copy_from_slice(&acc);
        }
    }

    fn run_1x1_generic(&self, input: &[f32], in_stride: usize, n: usize, out: &mut [f32]) {
        let (cin, cout) = (self.cin, self.cout);
        for t in 0..n {
            let x = &input[t * in_stride..t * in_stride + cin];
            let o = &mut out[t * cout..(t + 1) * cout];
            for oc in 0..cout {
                let wr = &self.w[oc * cin..(oc + 1) * cin];
                let mut acc = self.bias[oc];
                for ic in 0..cin {
                    acc += wr[ic] * x[ic];
                }
                o[oc] = acc;
            }
        }
    }
}

/// Input history for a dilated conv: a linear buffer that is shifted down
/// only when full, so reads are always contiguous.
#[derive(Clone)]
struct Hist {
    c: usize,
    look: usize,
    buf: Vec<f32>,
    wpos: usize,
}

impl Hist {
    fn new(c: usize, look: usize) -> Hist {
        Hist { c, look, buf: vec![0.0; (look + HIST_SLACK + BLOCK) * c], wpos: look }
    }

    fn push(&mut self, data: &[f32], n: usize) {
        let c = self.c;
        if (self.wpos + n) * c > self.buf.len() {
            let src = (self.wpos - self.look) * c;
            self.buf.copy_within(src..self.wpos * c, 0);
            self.wpos = self.look;
        }
        self.buf[self.wpos * c..(self.wpos + n) * c].copy_from_slice(&data[..n * c]);
    }

    #[inline(always)]
    fn frame(&self, t: usize, lookback: usize) -> &[f32] {
        let f = self.wpos + t - lookback;
        &self.buf[f * self.c..(f + 1) * self.c]
    }

    fn advance(&mut self, n: usize) {
        self.wpos += n;
    }
}

#[derive(Clone)]
struct Layer {
    conv: Conv,
    hist: Hist,
    mixin: Conv,
    act: Act,
    secondary: Act,
    gating: Gating,
    bottleneck: usize,
    layer1x1: Option<Conv>,
    head1x1: Option<Conv>,
    // Scratch buffers (frame-major).
    z: Vec<f32>,
    mix: Vec<f32>,
    l1: Vec<f32>,
    head: Vec<f32>,
}

#[derive(Clone)]
struct LayerArray {
    channels: usize,
    head_in: usize,
    rechannel: Conv,
    layers: Vec<Layer>,
    head_conv: Conv,
    head_hist: Hist,
    // Scratch
    a: Vec<f32>,
    b: Vec<f32>,
    head_acc: Vec<f32>,
    head_out: Vec<f32>,
}

struct LayerArrayCfg {
    input_size: usize,
    condition_size: usize,
    channels: usize,
    bottleneck: usize,
    kernel_sizes: Vec<usize>,
    dilations: Vec<usize>,
    acts: Vec<Act>,
    gatings: Vec<Gating>,
    secondaries: Vec<Act>,
    groups_input: usize,
    groups_mixin: usize,
    layer1x1: Option<usize>,
    head1x1: Option<(usize, usize)>,
    head_size: usize,
    head_kernel: usize,
    head_bias: bool,
    head_dilation: usize,
}

fn uint(v: &Value, key: &str) -> Result<usize> {
    v.get(key).and_then(Value::as_u64).map(|x| x as usize).ok_or_else(|| anyhow!("missing/invalid `{key}`"))
}

fn parse_layer_array(lc: &Value) -> Result<LayerArrayCfg> {
    for key in [
        "conv_pre_film",
        "conv_post_film",
        "input_mixin_pre_film",
        "input_mixin_post_film",
        "activation_pre_film",
        "activation_post_film",
        "layer1x1_post_film",
        "head1x1_post_film",
    ] {
        if lc.get(key).and_then(|f| f.get("active")).and_then(Value::as_bool) == Some(true) {
            bail!("FiLM conditioning ({key}) is not supported");
        }
    }
    if lc.get("slimmable").is_some_and(|s| !s.is_null()) {
        bail!("per-layer slimmable configs are not supported");
    }
    let channels = uint(lc, "channels")?;
    let bottleneck = lc.get("bottleneck").and_then(Value::as_u64).map_or(channels, |b| b as usize);
    let dilations: Vec<usize> = lc
        .get("dilations")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing dilations"))?
        .iter()
        .map(|d| d.as_u64().map(|d| d as usize).ok_or_else(|| anyhow!("bad dilation")))
        .collect::<Result<_>>()?;
    let n = dilations.len();
    let kernel_sizes = if let Some(ks) = lc.get("kernel_sizes").and_then(Value::as_array) {
        ks.iter().map(|k| k.as_u64().map(|k| k as usize).ok_or_else(|| anyhow!("bad kernel size"))).collect::<Result<Vec<_>>>()?
    } else {
        vec![uint(lc, "kernel_size")?; n]
    };
    if kernel_sizes.len() != n {
        bail!("kernel_sizes length mismatch");
    }
    let per_layer = |key: &str, default: Option<&Value>| -> Result<Vec<Value>> {
        match lc.get(key).or(default) {
            Some(Value::Array(a)) => {
                if a.len() != n {
                    bail!("{key} length mismatch");
                }
                Ok(a.clone())
            }
            Some(v) => Ok(vec![v.clone(); n]),
            None => Ok(vec![Value::Null; n]),
        }
    };
    let acts = per_layer("activation", None)?.iter().map(Act::parse).collect::<Result<Vec<_>>>()?;
    let secondaries = per_layer("secondary_activation", None)?.iter().map(Act::parse).collect::<Result<Vec<_>>>()?;
    let gatings = if lc.get("gating_mode").is_some() {
        per_layer("gating_mode", None)?
            .iter()
            .map(|g| match g.as_str() {
                Some("none") => Ok(Gating::None),
                Some("gated") => Ok(Gating::Gated),
                Some("blended") => Ok(Gating::Blended),
                _ => bail!("bad gating_mode {g}"),
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        let gated = lc.get("gated").and_then(Value::as_bool).unwrap_or(false);
        vec![if gated { Gating::Gated } else { Gating::None }; n]
    };
    let layer1x1 = match lc.get("layer1x1") {
        Some(l) if l.get("active").and_then(Value::as_bool) == Some(false) => None,
        Some(l) => Some(uint(l, "groups").unwrap_or(1)),
        None => Some(1),
    };
    let head1x1 = match lc.get("head1x1") {
        Some(h) if h.get("active").and_then(Value::as_bool) == Some(true) => {
            Some((uint(h, "out_channels")?, uint(h, "groups").unwrap_or(1)))
        }
        _ => None,
    };
    let (head_size, head_kernel, head_bias, head_dilation) = match lc.get("head") {
        Some(h) if h.is_object() => (
            uint(h, "out_channels")?,
            uint(h, "kernel_size")?,
            h.get("bias").and_then(Value::as_bool).unwrap_or(false),
            h.get("head_dilation").and_then(Value::as_u64).map_or(1, |d| d as usize),
        ),
        _ => (uint(lc, "head_size")?, 1, lc.get("head_bias").and_then(Value::as_bool).unwrap_or(false), 1),
    };
    Ok(LayerArrayCfg {
        input_size: uint(lc, "input_size")?,
        condition_size: uint(lc, "condition_size")?,
        channels,
        bottleneck,
        kernel_sizes,
        dilations,
        acts,
        gatings,
        secondaries,
        groups_input: lc.get("groups_input").and_then(Value::as_u64).map_or(1, |g| g as usize),
        groups_mixin: lc.get("groups_input_mixin").and_then(Value::as_u64).map_or(1, |g| g as usize),
        layer1x1,
        head1x1,
        head_size,
        head_kernel,
        head_bias,
        head_dilation,
    })
}

impl LayerArray {
    fn new(c: &LayerArrayCfg, ws: &mut Weights) -> Result<LayerArray> {
        let rechannel = Conv::new(c.input_size, c.channels, 1, 1, 1, false, ws)?;
        let mut layers = Vec::new();
        for i in 0..c.dilations.len() {
            let gating = c.gatings[i];
            let zc = if gating == Gating::None { c.bottleneck } else { 2 * c.bottleneck };
            let conv = Conv::new(c.channels, zc, c.kernel_sizes[i], c.dilations[i], c.groups_input, true, ws)?;
            let mixin = Conv::new(c.condition_size, zc, 1, 1, c.groups_mixin, false, ws)?;
            let layer1x1 = match c.layer1x1 {
                Some(g) => Some(Conv::new(c.bottleneck, c.channels, 1, 1, g, true, ws)?),
                None => {
                    if c.bottleneck != c.channels {
                        bail!("layer1x1 inactive requires bottleneck == channels");
                    }
                    None
                }
            };
            let head1x1 = match c.head1x1 {
                Some((oc, g)) => Some(Conv::new(c.bottleneck, oc, 1, 1, g, true, ws)?),
                None => None,
            };
            let hist = Hist::new(c.channels, conv.lookback());
            let hc = head1x1.as_ref().map_or(c.bottleneck, |h| h.cout);
            layers.push(Layer {
                hist,
                mixin,
                act: c.acts[i],
                secondary: c.secondaries[i],
                gating,
                bottleneck: c.bottleneck,
                layer1x1,
                head1x1,
                z: vec![0.0; BLOCK * zc],
                mix: vec![0.0; BLOCK * zc],
                l1: vec![0.0; BLOCK * c.channels],
                head: vec![0.0; BLOCK * hc],
                conv,
            });
        }
        let head_in = c.head1x1.map_or(c.bottleneck, |(oc, _)| oc);
        let head_conv = Conv::new(head_in, c.head_size, c.head_kernel, c.head_dilation, 1, c.head_bias, ws)?;
        let head_hist = Hist::new(head_in, head_conv.lookback());
        Ok(LayerArray {
            channels: c.channels,
            head_in,
            rechannel,
            head_hist,
            a: vec![0.0; BLOCK * c.channels],
            b: vec![0.0; BLOCK * c.channels],
            head_acc: vec![0.0; BLOCK * head_in],
            head_out: vec![0.0; BLOCK * c.head_size],
            head_conv,
            layers,
        })
    }

    fn receptive_field(&self) -> usize {
        self.layers.iter().map(|l| l.conv.lookback()).sum::<usize>() + self.head_conv.lookback()
    }

    /// `input` is frame-major with `in_c` channels; `cond` has one channel
    /// per frame (the raw guitar signal).  `head_prev` seeds the head sum.
    fn process(&mut self, input: &[f32], in_c: usize, cond: &[f32], head_prev: Option<&[f32]>, n: usize) {
        let ch = self.channels;
        self.rechannel.run_1x1(input, in_c, n, &mut self.a);
        match head_prev {
            Some(h) => self.head_acc[..n * self.head_in].copy_from_slice(&h[..n * self.head_in]),
            None => self.head_acc[..n * self.head_in].fill(0.0),
        }
        for layer in &mut self.layers {
            // `a` holds this layer's input; result goes to `b`, then swap.
            layer.hist.push(&self.a, n);
            layer.conv.run(&layer.hist, n, &mut layer.z);
            layer.hist.advance(n);
            let zc = layer.conv.cout;
            layer.mixin.run_1x1(cond, layer.mixin.cin, n, &mut layer.mix);
            let bn = layer.bottleneck;
            for t in 0..n {
                let z = &mut layer.z[t * zc..(t + 1) * zc];
                let m = &layer.mix[t * zc..(t + 1) * zc];
                for c in 0..zc {
                    z[c] += m[c];
                }
                match layer.gating {
                    Gating::None => match layer.act {
                        Act::LeakyRelu(s) => {
                            for v in z.iter_mut() {
                                *v = if *v > 0.0 { *v } else { *v * s };
                            }
                        }
                        act => {
                            for v in z.iter_mut() {
                                *v = act.apply(*v);
                            }
                        }
                    },
                    Gating::Gated => {
                        for c in 0..bn {
                            z[c] = layer.act.apply(z[c]) * layer.secondary.apply(z[c + bn]);
                        }
                    }
                    Gating::Blended => {
                        for c in 0..bn {
                            let alpha = layer.secondary.apply(z[c + bn]);
                            z[c] = alpha * layer.act.apply(z[c]) + (1.0 - alpha) * z[c];
                        }
                    }
                }
            }
            // Head contribution (skip connection).
            match &layer.head1x1 {
                Some(h) => {
                    h.run_1x1(&layer.z, zc, n, &mut layer.head);
                    for (acc, v) in self.head_acc[..n * self.head_in].iter_mut().zip(&layer.head) {
                        *acc += v;
                    }
                }
                None => {
                    for t in 0..n {
                        let acc = &mut self.head_acc[t * bn..(t + 1) * bn];
                        for c in 0..bn {
                            acc[c] += layer.z[t * zc + c];
                        }
                    }
                }
            }
            // Residual to the next layer.
            match &layer.layer1x1 {
                Some(l1) => {
                    l1.run_1x1(&layer.z, zc, n, &mut layer.l1);
                    for i in 0..n * ch {
                        self.b[i] = self.a[i] + layer.l1[i];
                    }
                    std::mem::swap(&mut self.a, &mut self.b);
                }
                None => {}
            }
        }
        self.head_hist.push(&self.head_acc, n);
        self.head_conv.run(&self.head_hist, n, &mut self.head_out);
        self.head_hist.advance(n);
    }
}

#[derive(Clone)]
struct WaveNet {
    arrays: Vec<LayerArray>,
    head_scale: f32,
    prewarm: usize,
}

impl WaveNet {
    fn from_json(model: &Value) -> Result<WaveNet> {
        let arch = model.get("architecture").and_then(Value::as_str).unwrap_or("");
        if arch != "WaveNet" {
            bail!("unsupported NAM architecture `{arch}` (only WaveNet is implemented)");
        }
        let cfg = model.get("config").ok_or_else(|| anyhow!("missing config"))?;
        if cfg.get("head").is_some_and(|h| !h.is_null()) {
            bail!("WaveNet post-stack heads are not supported");
        }
        if cfg.get("condition_dsp").is_some_and(|h| !h.is_null()) {
            bail!("condition_dsp is not supported");
        }
        let weights: Vec<f32> = model
            .get("weights")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("missing weights"))?
            .iter()
            .map(|w| w.as_f64().unwrap_or(0.0) as f32)
            .collect();
        let mut ws = Weights { w: &weights, i: 0 };
        let mut arrays = Vec::new();
        let layer_cfgs = cfg.get("layers").and_then(Value::as_array).ok_or_else(|| anyhow!("missing layers"))?;
        for (i, lc) in layer_cfgs.iter().enumerate() {
            let c = parse_layer_array(lc).with_context(|| format!("layer array {i}"))?;
            if c.condition_size != 1 {
                bail!("only mono conditioning is supported");
            }
            arrays.push(LayerArray::new(&c, &mut ws)?);
        }
        if arrays.is_empty() {
            bail!("model has no layer arrays");
        }
        if arrays.last().unwrap().head_conv.cout != 1 {
            bail!("only mono-output models are supported");
        }
        let head_scale = ws.take()?;
        if ws.i != weights.len() {
            bail!("weight count mismatch: used {} of {}", ws.i, weights.len());
        }
        let prewarm = 1 + arrays.iter().map(LayerArray::receptive_field).sum::<usize>();
        Ok(WaveNet { arrays, head_scale, prewarm })
    }

    fn process_block(&mut self, input: &[f32], output: &mut [f32]) {
        let n = input.len();
        for i in 0..self.arrays.len() {
            let (done, rest) = self.arrays.split_at_mut(i);
            let la = &mut rest[0];
            if i == 0 {
                la.process(input, 1, input, None, n);
            } else {
                let prev = &done[i - 1];
                la.process(&prev.a, prev.channels, input, Some(&prev.head_out), n);
            }
        }
        let out = &self.arrays.last().unwrap().head_out;
        for (o, v) in output.iter_mut().zip(out) {
            *o = self.head_scale * v;
        }
    }
}

/// A loaded NAM capture, ready to process mono audio at [`Self::sample_rate`].
#[derive(Clone)]
pub struct NamModel {
    net: WaveNet,
    pub sample_rate: f32,
    /// Measured loudness of the capture (dB), if present in the metadata.
    pub loudness: Option<f32>,
    pub name: String,
}

impl NamModel {
    pub fn load(path: &Path) -> Result<NamModel> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let json: Value = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        let mut m = Self::from_json(&json)?;
        if m.name.is_empty() {
            m.name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        }
        Ok(m)
    }

    pub fn from_json(json: &Value) -> Result<NamModel> {
        let meta = json.get("metadata");
        let loudness = meta.and_then(|m| m.get("loudness")).and_then(Value::as_f64).map(|l| l as f32);
        let name = meta.and_then(|m| m.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
        let sample_rate = json.get("sample_rate").and_then(Value::as_f64).unwrap_or(48000.0) as f32;
        let arch = json.get("architecture").and_then(Value::as_str).unwrap_or("");
        let model = if arch == "SlimmableContainer" {
            // Use the highest-quality submodel (largest max_value), matching
            // the reference container's default.
            json.pointer("/config/submodels")
                .and_then(Value::as_array)
                .and_then(|s| {
                    s.iter().max_by(|a, b| {
                        let f = |v: &Value| v.get("max_value").and_then(Value::as_f64).unwrap_or(0.0);
                        f(a).total_cmp(&f(b))
                    })
                })
                .and_then(|s| s.get("model"))
                .ok_or_else(|| anyhow!("SlimmableContainer without submodels"))?
        } else {
            json
        };
        let mut net = WaveNet::from_json(model)?;
        let mut zeros = [0.0f32; BLOCK];
        let mut left = net.prewarm;
        while left > 0 {
            let n = left.min(BLOCK);
            let z = [0.0f32; BLOCK];
            net.process_block(&z[..n], &mut zeros[..n]);
            left -= n;
        }
        Ok(NamModel { net, sample_rate, loudness, name })
    }

    /// Samples of input history that affect the current output.
    pub fn receptive_field(&self) -> usize {
        self.net.prewarm
    }

    /// Process mono audio; `input` and `output` must have equal length.
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        for (i, o) in input.chunks(BLOCK).zip(output.chunks_mut(BLOCK)) {
            self.net.process_block(i, o);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-built single-layer model whose output can be computed by hand.
    #[test]
    fn tiny_model_matches_hand_calculation() {
        // 1 array: input 1 -> channels 1, one layer k=2 d=1, no gating,
        // ReLU, layer1x1, head 1x1 (legacy head_size).
        let model = serde_json::json!({
            "architecture": "WaveNet",
            "sample_rate": 48000,
            "config": {
                "layers": [{
                    "input_size": 1, "condition_size": 1, "channels": 1, "head_size": 1,
                    "kernel_size": 2, "dilations": [1], "activation": "ReLU",
                    "gated": false, "head_bias": false
                }],
                "head_scale": 0.5
            },
            // rechannel w; conv w(k0), w(k1), bias; mixin w; layer1x1 w, b; head w; head_scale
            "weights": [2.0, 0.5, 1.0, 0.1, 0.0, 1.0, 0.0, 3.0, 0.5]
        });
        let mut m = NamModel::from_json(&model).unwrap();
        let input = [1.0, 0.0, -1.0, 2.0];
        let mut out = [0.0; 4];
        m.process(&input, &mut out);
        // x' = 2x; z = relu(0.5*x'[t-1] + 1.0*x'[t] + 0.1); y = 0.5 * 3 * z
        let xs = [0.0, 2.0, 0.0, -2.0, 4.0];
        for t in 0..4 {
            let z: f32 = (0.5 * xs[t] + xs[t + 1] + 0.1f32).max(0.0);
            assert!((out[t] - 1.5 * z).abs() < 1e-6, "t={t}: {} vs {}", out[t], 1.5 * z);
        }
    }

    /// Runs only when a real `.nam` capture is available.
    #[test]
    fn real_capture_through_amp_chain() {
        let Some(path) = find_model() else {
            eprintln!("skipping: no .nam model found");
            return;
        };
        let model = NamModel::load(&path).unwrap();
        use crate::model::ToneKind;
        for (tone, stereo) in [(ToneKind::Clean, false), (ToneKind::Nam, false), (ToneKind::Nam, true)] {
            let mut chain = crate::synth::amp::AmpChain::with_nam(&model, 48000.0, tone, stereo);
            let (mut energy, mut peak) = (0.0f64, 0.0f32);
            for b in 0..60 {
                let mut l: Vec<f32> =
                    (0..256).map(|i| 0.3 * (std::f32::consts::TAU * 82.4 * (b * 256 + i) as f32 / 48000.0).sin()).collect();
                let mut r = l.clone();
                chain.process_block(&mut l, &mut r);
                for &y in l.iter().chain(&r) {
                    assert!(y.is_finite());
                    energy += (y * y) as f64;
                    peak = peak.max(y.abs());
                }
            }
            assert!(energy > 1.0, "{tone:?} stereo={stereo}: NAM chain output is silent");
            assert!(peak < 4.0, "{tone:?} stereo={stereo}: NAM chain output level runaway ({peak})");
        }
    }
}

/// Locate a `.nam` capture: `$GPT_NAM_MODEL`, else the first `*.nam` file in the sound
/// directories, their `nam/` subfolders, or the folder that contains `assets/`.
pub fn find_model() -> Option<std::path::PathBuf> {
    if let Some(p) = std::env::var_os("GPT_NAM_MODEL") {
        return Some(p.into());
    }
    if let Some(p) = crate::setup::Settings::load().nam_model.filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut dirs = Vec::new();
    for d in super::sampler::search_dirs() {
        dirs.push(d.join("nam"));
        if let Some(parent) = d.parent() {
            dirs.push(if parent.as_os_str().is_empty() { ".".into() } else { parent.to_path_buf() });
        }
        dirs.push(d);
    }
    dirs.into_iter().find_map(|d| {
        let mut files: Vec<_> = std::fs::read_dir(d)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("nam")))
            .collect();
        files.sort();
        files.into_iter().next()
    })
}
