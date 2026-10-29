//! Backing track: decode a song's audio and line it up with the score automatically.
//!
//! The audio can be a single mixdown, but it works much better with *stems* — the separated
//! drums / bass / guitar / vocal tracks that tools like Demucs produce. Stems all share one
//! time scale, so the song only has to be lined up once, and each stem can be matched against
//! the matching tracks in the tab (drums against the drum part, bass against the bass part),
//! which is far more reliable than matching one dense full mix.
//!
//! The recording is the master clock. It is never sped up or slowed down; instead the tab is
//! bent to follow it. Alignment runs in two passes:
//!
//! 1. *Structure*: the whole tab is matched against the whole recording by what each part is
//!    doing - which instruments are playing and which notes - with dynamic time warping. That is
//!    what tells a riff from the same riff eight bars later, and it assumes nothing about where
//!    the tab starts or ends in the recording, or about the band holding one tempo.
//! 2. *Hits*: with the bars settled, each stretch of the song is nudged a fraction of a beat so
//!    the tab's hits land on the recording's.
//!
//! The result is a map from score time to recording time that playback follows, plus checks all
//! the way through the song, so a recording of a different arrangement is reported instead of
//! silently sounding wrong.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::timeline::{NoteEvent, Schedule};

/// Frame rate of the onset envelopes used for alignment.
const HZ: f64 = 100.0;
/// Hits landing this close read as one performance rather than two.
const GOOD_HIT: f64 = 0.04;
/// Minimum correlation for a window to count as a confident match.
const CHECK_CONF: f32 = 0.08;
/// Minimum correlation for the whole-song fit to be trusted at all.
const GLOBAL_CONF: f32 = 0.04;
/// Drift beyond this (seconds) is audible and reported as a problem.
const DRIFT_BAD: f64 = 0.12;
/// Drift beyond this (seconds) is worth mentioning.
const DRIFT_WARN: f64 = 0.05;
/// Most stems that can be loaded at once.
pub const MAX_STEMS: usize = 8;

/// File extensions the audio decoder accepts.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "m4a", "aac", "aiff", "aif", "caf", "opus"];

// ---------------------------------------------------------------------------
// Stems
// ---------------------------------------------------------------------------

/// What a stem holds, which decides the part of the tab it is matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StemKind {
    Drums,
    Bass,
    Guitar,
    /// Everything that is not drums, bass or vocals (guitars, keys).
    Other,
    Vocals,
    /// A complete mixdown.
    Mix,
}

impl StemKind {
    /// Every role, in the order the picker offers them.
    pub const ALL: [StemKind; 6] =
        [StemKind::Drums, StemKind::Bass, StemKind::Guitar, StemKind::Other, StemKind::Vocals, StemKind::Mix];

    /// What this role is matched against, for the picker's tooltip.
    pub fn hint(self) -> &'static str {
        match self {
            StemKind::Drums => "Matched against the tab's drum track.",
            StemKind::Bass => "Matched against the tab's bass track.",
            StemKind::Guitar | StemKind::Other => "Matched against everything in the tab that is not drums or bass.",
            StemKind::Vocals => "A tab has no vocal line, so vocals take no part in the lining up.",
            StemKind::Mix => "Everything at once. Matched against the whole tab.",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            StemKind::Drums => "Drums",
            StemKind::Bass => "Bass",
            StemKind::Guitar => "Guitar",
            StemKind::Other => "Other",
            StemKind::Vocals => "Vocals",
            StemKind::Mix => "Full mix",
        }
    }

    /// Guess what a stem holds from its file name, the way separation tools name them
    /// (`song [drums].mp3`, `bass.wav`, `htdemucs/song/vocals.mp3`, …).
    pub fn from_name(name: &str) -> StemKind {
        let n = name.to_ascii_lowercase();
        // "no_vocals" and friends are mixes, not vocal stems, so they are tested first.
        if n.contains("no_vocal") || n.contains("no vocal") || n.contains("instrumental") {
            return StemKind::Mix;
        }
        for (needle, kind) in [
            ("drum", StemKind::Drums),
            ("percussion", StemKind::Drums),
            ("bass", StemKind::Bass),
            ("guitar", StemKind::Guitar),
            ("gtr", StemKind::Guitar),
            ("vocal", StemKind::Vocals),
            ("vox", StemKind::Vocals),
            ("other", StemKind::Other),
            ("piano", StemKind::Other),
            ("keys", StemKind::Other),
        ] {
            if n.contains(needle) {
                return kind;
            }
        }
        StemKind::Mix
    }

    /// Stems with no counterpart in the tab can't help line anything up.
    fn alignable(self) -> bool {
        self != StemKind::Vocals
    }
}

/// One decoded audio file, kept at its own sample rate as interleaved 16-bit stereo (half the
/// memory of floats, and the sources are lossy to begin with).
pub struct Stem {
    pub path: PathBuf,
    pub name: String,
    pub kind: StemKind,
    pub sr: u32,
    /// Shared so a set can be lined up again under different roles without copying the audio.
    samples: Arc<Vec<i16>>,
    /// How well this stem on its own matched the tab, 0..1.
    pub confidence: f32,
    /// Median distance from a tab hit to the recording's nearest hit, in seconds.
    pub error: f64,
    /// Share of the tab's hits for this part that found a hit in the recording.
    pub found: f32,
}

impl Stem {
    /// A stem from interleaved stereo samples, for tests.
    #[cfg(test)]
    pub(crate) fn from_samples(sr: u32, samples: Vec<i16>) -> Stem {
        Stem {
            path: PathBuf::new(),
            name: String::new(),
            kind: StemKind::Mix,
            sr,
            samples: Arc::new(samples),
            confidence: 1.0,
            error: 0.0,
            found: 1.0,
        }
    }

    pub fn view(&self) -> Audio<'_> {
        Audio { sr: self.sr, samples: &self.samples }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration(&self) -> f64 {
        self.frames() as f64 / self.sr.max(1) as f64
    }

    /// How well the tab's hits for this part land on the recording's, in words, and whether
    /// that is good enough to play along to. This leads with the timing error rather than the
    /// correlation behind it: a synthesised tab and a real band never agree sample for sample,
    /// so the correlation reads far worse than the result is, while milliseconds can be judged
    /// against what an ear will accept.
    pub fn agreement(&self) -> (String, bool) {
        if !self.kind.alignable() {
            return ("not used for lining up".into(), false);
        }
        if !self.error.is_finite() || self.found < 0.2 {
            return ("too few clear hits to judge".into(), false);
        }
        let ms = self.error * 1000.0;
        let word = match ms {
            m if m <= 40.0 => "hits land together",
            m if m <= 80.0 => "hits land close",
            m if m <= 150.0 => "hits land loosely",
            _ => "hits do not land together",
        };
        (format!("{word} (within {ms:.0}ms, {:.0}% matched)", self.found * 100.0), ms <= 80.0)
    }

    pub fn memory_bytes(&self) -> usize {
        self.samples.len() * 2
    }

    /// Linearly interpolated stereo sample at `t` seconds into the recording.
    #[inline]
    pub fn sample_at(&self, t: f64) -> (f32, f32) {
        if t < 0.0 {
            return (0.0, 0.0);
        }
        let x = t * self.sr as f64;
        let i = x as usize;
        if i + 1 >= self.frames() {
            return (0.0, 0.0);
        }
        let f = (x - i as f64) as f32;
        let s = &self.samples[i * 2..i * 2 + 4];
        let lerp = |a: i16, b: i16| (a as f32 + (b as f32 - a as f32) * f) * (1.0 / 32768.0);
        (lerp(s[0], s[2]), lerp(s[1], s[3]))
    }
}

/// A set of stems that share one time scale, plus the alignment that maps the tab onto them.
pub struct BackingSet {
    pub stems: Vec<Stem>,
    pub align: Alignment,
}

impl BackingSet {
    /// Position in the recording for a position in the score, both in seconds.
    #[inline]
    pub fn audio_time(&self, score_secs: f64) -> f64 {
        self.align.audio_time(score_secs)
    }

    /// Position in the score for a position in the recording, both in seconds.
    #[inline]
    pub fn score_time(&self, audio_secs: f64) -> f64 {
        self.align.score_time(audio_secs)
    }

    pub fn memory_bytes(&self) -> usize {
        self.stems.iter().map(|s| s.memory_bytes()).sum()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.stems.iter().map(|s| s.path.clone()).collect()
    }

    /// Line the set up again with different roles, reusing the audio already in memory. A
    /// stem's role decides the part of the tab it is matched against, so changing one means
    /// measuring the whole set again - but not re-reading a few hundred megabytes to do it.
    pub fn with_roles(&self, kinds: &[StemKind], sched: &Schedule) -> BackingSet {
        let mut stems: Vec<Stem> = self
            .stems
            .iter()
            .enumerate()
            .map(|(i, s)| Stem {
                path: s.path.clone(),
                name: s.name.clone(),
                kind: kinds.get(i).copied().unwrap_or(s.kind),
                sr: s.sr,
                samples: s.samples.clone(),
                confidence: 0.0,
                error: f64::NAN,
                found: 0.0,
            })
            .collect();
        let refs: Vec<(StemKind, Audio)> = stems.iter().map(|s| (s.kind, s.view())).collect();
        let (align, per_stem) = align(sched, &refs);
        for (stem, fit) in stems.iter_mut().zip(per_stem) {
            stem.confidence = fit.confidence;
            stem.error = fit.error;
            stem.found = fit.found;
        }
        BackingSet { stems, align }
    }

    /// Decode every file and line the whole set up against `sched`. `progress` is called with a
    /// short status line as each file is read.
    pub fn load(paths: &[PathBuf], sched: &Schedule, progress: impl Fn(String)) -> Result<BackingSet, String> {
        if paths.is_empty() {
            return Err("No audio files were chosen.".into());
        }
        if sched.events.is_empty() {
            return Err("This tab has no notes to line the audio up against.".into());
        }
        let paths = &paths[..paths.len().min(MAX_STEMS)];
        let mut decoded = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let name = file_name(path);
            progress(format!("Reading {name} ({}/{})…", i + 1, paths.len()));
            let audio = decode(path).map_err(|e| format!("{name}: {e}"))?;
            if audio.frames() < audio.sr as usize {
                return Err(format!("{name} is shorter than a second."));
            }
            decoded.push((path.clone(), name, audio));
        }
        // One file is a mixdown whatever it happens to be called; several are stems.
        let kinds: Vec<StemKind> = if decoded.len() == 1 {
            vec![StemKind::Mix]
        } else {
            decoded.iter().map(|(_, n, _)| StemKind::from_name(n)).collect()
        };

        progress("Lining the audio up with the tab…".into());
        let refs: Vec<(StemKind, Audio)> = kinds.iter().copied().zip(decoded.iter().map(|(_, _, a)| a.view())).collect();
        let (align, per_stem) = align(sched, &refs);

        let stems = decoded
            .into_iter()
            .zip(kinds)
            .zip(per_stem)
            .map(|(((path, name, audio), kind), fit)| Stem {
                path,
                name,
                kind,
                sr: audio.sr,
                samples: Arc::new(audio.samples),
                confidence: fit.confidence,
                error: fit.error,
                found: fit.found,
            })
            .collect();
        Ok(BackingSet { stems, align })
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Load a set of stems on a worker thread.
pub fn load_async(
    paths: Vec<PathBuf>,
    sched: Arc<Schedule>,
    progress: impl Fn(String) + Send + 'static,
    done: impl FnOnce(Result<BackingSet, String>) + Send + 'static,
) {
    std::thread::Builder::new()
        .name("backing-load".into())
        .spawn(move || done(BackingSet::load(&paths, &sched, progress)))
        .ok();
}

// ---------------------------------------------------------------------------
// Alignment report
// ---------------------------------------------------------------------------

/// How well the recording lines up with the tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlignStatus {
    /// Lines up all the way through.
    Good,
    /// Usable, but some parts drift or couldn't be verified.
    Warn,
    /// No trustworthy alignment was found.
    Failed,
}

/// One verification point somewhere in the song.
#[derive(Debug, Clone, Copy)]
pub struct Check {
    /// Score time of the window centre, in seconds.
    pub time: f64,
    /// Bar number at that point.
    pub bar: usize,
    /// How far lining up the hits moved the tab from where the structure match put it, in
    /// seconds.
    pub drift: f64,
    pub confidence: f32,
    /// False when the window held too little to match (silence, long rests).
    pub matched: bool,
}

/// The fitted mapping from score time to recording time, and how it was verified.
#[derive(Debug, Clone)]
pub struct Alignment {
    /// Seconds into the recording where the tab's bar 1 begins.
    pub offset: f64,
    /// Recording seconds per score second: above 1 means the band plays slower than the tab.
    pub rate: f64,
    pub confidence: f32,
    /// Mean of the stems' median hit errors, in seconds: how late or early the tab's hits land
    /// against the recording's once the map is applied. NaN when nothing could be compared.
    pub hit_error: f64,
    /// Which part gave `hit_error`, for saying so out loud.
    pub hit_part: String,
    pub checks: Vec<Check>,
    /// Largest such nudge across all confident checks, in seconds.
    pub max_drift: f64,
    pub status: AlignStatus,
    /// One line, for the transport bar and banners.
    pub summary: String,
    /// The full explanation, for the backing-track dialog.
    pub detail: String,
    /// Points of `(score seconds, recording seconds)` that playback follows between. A band
    /// never holds one exact tempo for five minutes, so a single straight line slowly slides
    /// out of time; these let the tab follow the recording wherever it actually went.
    pub map: Vec<(f64, f64)>,
}

impl Alignment {
    pub fn ok(&self) -> bool {
        self.status != AlignStatus::Failed
    }

    /// Position in the recording for a position in the score, both in seconds. Follows the
    /// measured map where there is one and the overall tempo beyond its ends.
    #[inline]
    pub fn audio_time(&self, score_secs: f64) -> f64 {
        if self.map.is_empty() {
            return self.offset + score_secs * self.rate;
        }
        interp(&self.map, score_secs, self.rate, false)
    }

    /// Position in the score for a position in the recording: the other way round from
    /// `audio_time`, for playback that lets the recording keep time.
    #[inline]
    pub fn score_time(&self, audio_secs: f64) -> f64 {
        if self.map.is_empty() {
            return (audio_secs - self.offset) / self.rate;
        }
        interp(&self.map, audio_secs, 1.0 / self.rate, true)
    }

    /// How much faster than the tab the recording runs over its slowest and fastest stretches, in
    /// percent, ignoring the odd extreme. Measured over ten-second spans, so it is the band's
    /// tempo rather than a single slipped beat.
    pub fn tempo_range(&self) -> (f64, f64) {
        let end = self.map.last().map(|p| p.0).unwrap_or(0.0);
        let mut v = Vec::new();
        let mut x = 0.0;
        while x + 10.0 <= end {
            let r = (self.audio_time(x + 10.0) - self.audio_time(x)) / 10.0;
            if r > 0.0 {
                v.push((1.0 / r - 1.0) * 100.0);
            }
            x += 5.0;
        }
        if v.is_empty() {
            return (self.tempo_pct(), self.tempo_pct());
        }
        v.sort_by(f64::total_cmp);
        (v[v.len() * 5 / 100], v[(v.len() * 95 / 100).min(v.len() - 1)])
    }

    /// Tempo difference in percent: positive means the recording is faster than the tab.
    pub fn tempo_pct(&self) -> f64 {
        (1.0 / self.rate - 1.0) * 100.0
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

pub struct Decoded {
    pub sr: u32,
    /// Interleaved stereo.
    pub samples: Vec<i16>,
}

impl Decoded {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn view(&self) -> Audio<'_> {
        Audio { sr: self.sr, samples: &self.samples }
    }
}

/// A borrowed look at decoded audio, so alignment can run over freshly decoded files or over
/// stems a set is already holding, without copying a few hundred megabytes to do it.
#[derive(Clone, Copy)]
pub struct Audio<'a> {
    pub sr: u32,
    /// Interleaved stereo.
    pub samples: &'a [i16],
}

impl Audio<'_> {
    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    /// Mono sample, for analysis.
    #[inline]
    fn mono(&self, i: usize) -> f32 {
        (self.samples[i * 2] as f32 + self.samples[i * 2 + 1] as f32) * (0.5 / 32768.0)
    }
}

/// Decode an audio file to interleaved 16-bit stereo at its native sample rate.
pub fn decode(path: &Path) -> Result<Decoded, String> {
    use symphonia::core::codecs::CodecParameters;
    use symphonia::core::errors::Error;
    use symphonia::core::formats::TrackType;
    use symphonia::core::formats::probe::Hint;
    use symphonia::core::io::MediaSourceStream;

    let file = std::fs::File::open(path).map_err(|e| format!("couldn't open it ({e})"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mut reader = symphonia::default::get_probe()
        .probe(&hint, mss, Default::default(), Default::default())
        .map_err(|e| format!("unsupported audio file ({e})"))?;

    let track = reader
        .first_track_known_codec(TrackType::Audio)
        .ok_or_else(|| "it has no decodable audio track".to_string())?;
    let track_id = track.id;
    let params = match track.codec_params.as_ref() {
        Some(CodecParameters::Audio(a)) => a.clone(),
        _ => return Err("it has no decodable audio track".into()),
    };
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(&params, &Default::default())
        .map_err(|e| format!("no decoder for this audio format ({e})"))?;

    let mut samples: Vec<i16> = Vec::new();
    let mut scratch: Vec<i16> = Vec::new();
    let mut sr = params.sample_rate.unwrap_or(44_100);
    loop {
        let packet = match reader.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) => break,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("error while reading it ({e})")),
        };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            // A damaged frame here and there isn't worth failing the whole file over.
            Err(Error::DecodeError(_)) => continue,
            Err(Error::IoError(_)) => break,
            Err(e) => return Err(format!("error while decoding it ({e})")),
        };
        let spec = buf.spec().clone();
        sr = spec.rate();
        let ch = spec.channels().count().max(1);
        scratch.clear();
        buf.copy_to_vec_interleaved(&mut scratch);
        match ch {
            1 => samples.extend(scratch.iter().flat_map(|s| [*s, *s])),
            2 => samples.extend_from_slice(&scratch),
            _ => samples.extend(scratch.chunks(ch).flat_map(|f| [f[0], f[1]])),
        }
    }
    if samples.is_empty() {
        return Err("it decoded to nothing".into());
    }
    Ok(Decoded { sr, samples })
}

// ---------------------------------------------------------------------------
// Onset envelopes
// ---------------------------------------------------------------------------

/// Score onsets placed on the recording's time line through `at`, for refining a match that is
/// already close. `n` is the length of the envelope wanted.
fn score_onsets_at(sched: &Schedule, kind: StemKind, n: usize, at: &dyn Fn(f64) -> f64) -> Vec<f32> {
    let tl = &sched.timeline;
    let mut env = vec![0.0f32; n];
    let mut hits = 0;
    for e in &sched.events {
        if e.legato || !event_matches(sched, e, kind) {
            continue;
        }
        let t = at(tl.seconds_at(e.start as f64));
        if t < 0.0 {
            continue;
        }
        let k = (t * HZ).round() as usize;
        if k < env.len() {
            env[k] += e.velocity.max(0.05);
            hits += 1;
        }
    }
    if hits < 16 && kind != StemKind::Mix {
        return score_onsets_at(sched, StemKind::Mix, n, at);
    }
    shape(&mut env);
    env
}

/// Does this scheduled note belong to the part of the band a stem holds?
fn event_matches(sched: &Schedule, e: &NoteEvent, kind: StemKind) -> bool {
    let bass = sched.tracks.get(e.track).is_some_and(|t| t.tone.is_bass());
    match kind {
        StemKind::Drums => e.drum,
        StemKind::Bass => bass,
        // Separation tools fold guitars and keys together into "other".
        StemKind::Guitar | StemKind::Other => !e.drum && !bass,
        StemKind::Vocals | StemKind::Mix => true,
    }
}

/// Note onsets written in the tab for the part a stem holds, at `HZ` frames per second.
/// Falls back to the whole band when the tab has no matching track.
fn score_onsets(sched: &Schedule, kind: StemKind) -> Vec<f32> {
    let tl = &sched.timeline;
    let n = (tl.duration() * HZ).ceil() as usize + 2;
    let mut env = vec![0.0f32; n];
    let mut hits = 0;
    for e in &sched.events {
        // Hammer-ons and legato slides are not re-plucked, so they barely show up in a recording.
        if e.legato || !event_matches(sched, e, kind) {
            continue;
        }
        let k = (tl.seconds_at(e.start as f64) * HZ) as usize;
        if k < env.len() {
            env[k] += e.velocity.max(0.05);
            hits += 1;
        }
    }
    if hits < 16 && kind != StemKind::Mix {
        return score_onsets(sched, StemKind::Mix);
    }
    shape(&mut env);
    env
}

/// Onsets detected in a recording (spectral flux), at `HZ` frames per second.
fn audio_onsets(audio: Audio) -> Vec<f32> {
    const WIN: usize = 1024;
    let hop = ((audio.sr as f64 / HZ).round() as usize).max(1);
    let frames = audio.frames();
    let n = frames / hop;
    let mut env = vec![0.0f32; n];
    if n == 0 {
        return env;
    }

    let fft = Fft::new(WIN);
    let window: Vec<f64> =
        (0..WIN).map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / WIN as f64).cos()).collect();
    // Ignore everything above ~11 kHz: cymbal wash up there drowns the pitched onsets.
    let top = ((11_000.0 / audio.sr as f64 * WIN as f64) as usize).clamp(1, WIN / 2);

    let mut re = vec![0.0f64; WIN];
    let mut im = vec![0.0f64; WIN];
    let mut mag = vec![0.0f32; top];
    let mut prev = vec![0.0f32; top];
    for f in 0..n {
        // Centre the analysis window on the frame rather than starting it there, otherwise
        // every onset is reported half a window late and the whole track sits early by that
        // much once it is matched up.
        let start = f as isize * hop as isize - WIN as isize / 2;
        for i in 0..WIN {
            let j = start + i as isize;
            let s = if j >= 0 && (j as usize) < frames { audio.mono(j as usize) } else { 0.0 };
            re[i] = s as f64 * window[i];
            im[i] = 0.0;
        }
        fft.run(&mut re, &mut im, false);
        let mut flux = 0.0f32;
        for k in 0..top {
            // A log magnitude keeps quiet passages from being ignored entirely.
            mag[k] = (1.0 + 20.0 * (re[k] * re[k] + im[k] * im[k]).sqrt()).ln() as f32;
            let d = mag[k] - prev[k];
            if d > 0.0 {
                flux += d;
            }
        }
        prev.copy_from_slice(&mag);
        env[f] = flux;
    }

    // The hop has to be a whole number of samples, so at a rate like 11025 Hz the frames land
    // at 100.23 per second rather than HZ. Left alone that is a quarter of a percent of tempo
    // error baked into every measurement, so put the envelope back onto an exact HZ grid.
    let actual = audio.sr as f64 / hop as f64;
    if (actual - HZ).abs() > 1e-9 {
        let out_n = ((n as f64) * HZ / actual).floor() as usize;
        let mut out = vec![0.0f32; out_n];
        for (i, o) in out.iter_mut().enumerate() {
            let x = i as f64 * actual / HZ;
            let k = x as usize;
            let frac = (x - k as f64) as f32;
            let a = env.get(k).copied().unwrap_or(0.0);
            let b = env.get(k + 1).copied().unwrap_or(a);
            *o = a + (b - a) * frac;
        }
        env = out;
    }

    shape(&mut env);
    env
}

/// Smooth an onset envelope, take out the slow loudness contour and rectify it, so that two
/// envelopes from completely different sources can be compared with a plain correlation.
fn shape(env: &mut [f32]) {
    let n = env.len();
    if n == 0 {
        return;
    }
    // ~50 ms of smoothing. A band never plays exactly on the grid the tab is written on, and
    // without this tolerance a performance correlates no better than noise.
    const R: isize = 4;
    let sigma = 2.0f32;
    let kernel: Vec<f32> = (-R..=R).map(|x| (-(x * x) as f32 / (2.0 * sigma * sigma)).exp()).collect();
    let total: f32 = kernel.iter().sum();
    let src = env.to_vec();
    for i in 0..n {
        let mut acc = 0.0;
        for (k, w) in kernel.iter().enumerate() {
            let j = i as isize + k as isize - R;
            if j >= 0 && (j as usize) < n {
                acc += src[j as usize] * w;
            }
        }
        env[i] = acc / total;
    }
    // Subtract a 1 s moving average and keep only what rises above it.
    let half = (HZ * 0.5) as usize;
    let mut prefix = Vec::with_capacity(n + 1);
    prefix.push(0.0f64);
    for v in env.iter() {
        prefix.push(prefix[prefix.len() - 1] + *v as f64);
    }
    let src = env.to_vec();
    for i in 0..n {
        let lo = i.saturating_sub(half);
        let hi = (i + half + 1).min(n);
        let mean = ((prefix[hi] - prefix[lo]) / (hi - lo) as f64) as f32;
        env[i] = (src[i] - mean).max(0.0);
    }
}

// ---------------------------------------------------------------------------
// Correlation
// ---------------------------------------------------------------------------

/// A correlation score for every lag in `first ..= first + scores.len() - 1`, where a lag is how
/// many frames into the recording the score envelope starts.
#[derive(Clone)]
struct Curve {
    first: isize,
    scores: Vec<f32>,
}

impl Curve {
    fn get(&self, lag: isize) -> Option<f32> {
        usize::try_from(lag - self.first).ok().and_then(|i| self.scores.get(i).copied())
    }

    /// Best lag (in frames, interpolated) and its score.
    #[cfg(test)]
    fn peak(&self) -> Option<(f64, f32)> {
        let (i, best) = self.scores.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1))?;
        Some((self.first as f64 + i as f64 + sub_frame(&self.scores, i), *best))
    }
}

/// Parabolic interpolation around a correlation peak, for sub-frame precision.
fn sub_frame(scores: &[f32], i: usize) -> f64 {
    if i == 0 || i + 1 >= scores.len() {
        return 0.0;
    }
    let (y0, y1, y2) = (scores[i - 1], scores[i], scores[i + 1]);
    let d = 2.0 * (2.0 * y1 - y0 - y2);
    if d.abs() < 1e-9 { 0.0 } else { ((y0 - y2) / d).clamp(-1.0, 1.0) as f64 }
}

/// Running sums of a series and of its squares, so the mean and spread of any slice are free.
struct Sums {
    s1: Vec<f64>,
    s2: Vec<f64>,
}

impl Sums {
    fn of(v: &[f32]) -> Sums {
        let mut s1 = Vec::with_capacity(v.len() + 1);
        let mut s2 = Vec::with_capacity(v.len() + 1);
        s1.push(0.0);
        s2.push(0.0);
        for x in v {
            s1.push(s1[s1.len() - 1] + *x as f64);
            s2.push(s2[s2.len() - 1] + (*x as f64) * (*x as f64));
        }
        Sums { s1, s2 }
    }

    /// Sum and variance-sum of `lo..hi`, about that slice's own mean.
    fn slice(&self, lo: usize, hi: usize) -> (f64, f64) {
        let n = (hi - lo) as f64;
        let s1 = self.s1[hi] - self.s1[lo];
        let s2 = self.s2[hi] - self.s2[lo];
        (s1, s2 - s1 * s1 / n)
    }
}

/// Pearson correlation from the raw dot product of two overlapping slices. Taking out each
/// slice's own mean matters: without it a lag can win just by sitting over a quiet patch.
fn pearson(dot: f64, (sa, va): (f64, f64), (sb, vb): (f64, f64), n: usize) -> f32 {
    if va <= 1e-12 || vb <= 1e-12 {
        return 0.0;
    }
    ((dot - sa * sb / n as f64) / (va * vb).sqrt()) as f32
}

/// Direct normalised correlation of a short window `b` against `a`, for lags in `lo..=hi`.
fn correlate_window(a: &[f32], b: &[f32], lo: isize, hi: isize) -> Curve {
    let nb = b.len() as isize;
    let na = a.len() as isize;
    let pa = Sums::of(a);
    let pb = Sums::of(b);
    let sb = pb.slice(0, b.len());
    let mut scores = Vec::with_capacity((hi - lo + 1).max(0) as usize);
    for lag in lo..=hi {
        if lag < 0 || lag + nb > na {
            scores.push(0.0);
            continue;
        }
        let mut dot = 0.0f64;
        for (k, y) in b.iter().enumerate() {
            dot += a[(lag + k as isize) as usize] as f64 * *y as f64;
        }
        let sa = pa.slice(lag as usize, (lag + nb) as usize);
        scores.push(pearson(dot, sa, sb, b.len()));
    }
    Curve { first: lo, scores }
}

/// Weighted average of several correlation curves, over the lags they all cover. This is how the
/// stems vote: a lag only wins if it suits the drums *and* the bass *and* the guitar.
fn combine(curves: &[(f32, Curve)]) -> Option<Curve> {
    let total: f32 = curves.iter().map(|(w, _)| *w).sum();
    if curves.is_empty() || total <= 0.0 {
        return None;
    }
    let first = curves.iter().map(|(_, c)| c.first).max()?;
    let last = curves.iter().map(|(_, c)| c.first + c.scores.len() as isize - 1).min()?;
    if last < first {
        return None;
    }
    let scores = (first..=last)
        .map(|lag| curves.iter().map(|(w, c)| w * c.get(lag).unwrap_or(0.0)).sum::<f32>() / total)
        .collect();
    Some(Curve { first, scores })
}

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

/// Frame rate of the coarse, whole-song match. Ten frames a second is plenty to tell a verse
/// from a chorus, and keeps the match of a five-minute song to a few million cells.
const SHAPE_HZ: f64 = 10.0;
/// A stem counts as playing when it is within this many dB of its loud passages...
const ON_RANGE: f64 = 35.0;
/// ...ramping from silent to fully on over this many dB, so a fade is not a cliff.
const ON_RAMP: f64 = 15.0;
/// How much a wrong chord costs next to a part that should be playing but is not.
const CHROMA_WEIGHT: f32 = 1.0;
/// The structure match picks a pace for the recording every this many tab frames...
const KNOT: usize = 40;
/// ...between these many recording frames per knot: from two thirds of the tab's speed to half
/// as fast again.
const STEP_MIN: usize = 27;
const STEP_MAX: usize = 60;
/// Cost of changing pace by one step (2.5%) between knots, so a stretch that repeats is crossed
/// steadily instead of wandering, and a band speeding up through a song is still followed.
const TEMPO_CHANGE: f32 = 0.5;
/// Most a change of pace can cost, so a real pause or a cut in the recording can still be taken.
const TEMPO_JUMP: f32 = 15.0;
/// How far, in frames, the tab may hang off either end of the recording.
const TAB_SLACK: usize = 600;
/// Cost per frame of tab left unmatched at either end, when the recording starts after the tab
/// does or stops before it.
const SKIP: f32 = 0.5;
/// How far the hit-by-hit pass may move the structural match, in seconds.
const REFINE_RANGE: f64 = 0.25;
/// Length of each hit-by-hit window, in seconds of recording.
const REFINE_WINDOW: f64 = 8.0;
/// Step between hit-by-hit windows, in seconds of recording.
const REFINE_HOP: f64 = 2.0;

/// What one part of the band is doing, coarsely: whether it is playing at all, and which notes.
/// This is what lets a riff that repeats every two bars be told apart from the same riff eight
/// bars later - the drums have come in, or the bass has, or the chords underneath have moved.
struct Part {
    on: Vec<f32>,
    chroma: Option<Vec<[f32; 12]>>,
}

/// The frequencies worth reading notes from in each kind of stem. Drums have no notes to speak
/// of, and a voice has nothing in the tab to compare with.
fn chroma_range(kind: StemKind) -> Option<(f64, f64)> {
    match kind {
        StemKind::Drums | StemKind::Vocals => None,
        StemKind::Bass => Some((40.0, 400.0)),
        StemKind::Guitar | StemKind::Other | StemKind::Mix => Some((70.0, 2500.0)),
    }
}

/// Compress and normalise one frame of pitch classes, the same way on both sides, so a tab's
/// velocities and a recording's magnitudes end up comparable with a dot product.
fn norm_chroma(c: &mut [f32; 12]) {
    let max = c.iter().copied().fold(0.0f32, f32::max);
    if max <= 1e-9 {
        *c = [0.0; 12];
        return;
    }
    for v in c.iter_mut() {
        *v = (1.0 + 10.0 * *v / max).ln();
    }
    let len = c.iter().map(|v| v * v).sum::<f32>().sqrt();
    if len > 1e-9 {
        for v in c.iter_mut() {
            *v /= len;
        }
    }
}

fn smooth_chroma(ch: &mut [[f32; 12]], r: usize) {
    let src = ch.to_vec();
    let n = src.len();
    for (i, out) in ch.iter_mut().enumerate() {
        let mut acc = [0.0f32; 12];
        for f in &src[i.saturating_sub(r)..(i + r + 1).min(n)] {
            for k in 0..12 {
                acc[k] += f[k];
            }
        }
        *out = acc;
    }
}

/// The part of the tab a stem holds, at `SHAPE_HZ`. None when the tab has too little of it.
fn tab_part(sched: &Schedule, kind: StemKind) -> Option<Part> {
    let tl = &sched.timeline;
    let n = (tl.duration() * SHAPE_HZ).ceil() as usize + 1;
    let mut on = vec![0.0f32; n];
    let mut chroma = vec![[0.0f32; 12]; n];
    let mut count = 0;
    for e in &sched.events {
        if !event_matches(sched, e, kind) {
            continue;
        }
        let s = tl.seconds_at(e.start as f64);
        let end = if e.drum { s + 0.25 } else { tl.seconds_at(e.end as f64).max(s + 0.1) };
        let a = (s * SHAPE_HZ) as usize;
        if a >= n {
            continue;
        }
        let b = ((end * SHAPE_HZ) as usize).min(n - 1);
        count += 1;
        let pc = (!e.drum && !e.dead).then(|| (e.pitch.round() as i32).rem_euclid(12) as usize);
        for f in a..=b {
            on[f] = 1.0;
            if let Some(pc) = pc {
                chroma[f][pc] += e.velocity.max(0.05);
            }
        }
    }
    if count < 16 {
        return None;
    }
    // A short rest in the tab is still the part playing, as far as a recording's loudness goes.
    let src = on.clone();
    for (i, v) in on.iter_mut().enumerate() {
        *v = src[i.saturating_sub(2)..(i + 3).min(n)].iter().copied().fold(0.0, f32::max);
    }
    let chroma = chroma_range(kind).map(|_| {
        smooth_chroma(&mut chroma, 2);
        chroma.iter_mut().for_each(norm_chroma);
        chroma
    });
    Some(Part { on, chroma })
}

/// What a stem of the recording is doing, at `SHAPE_HZ`.
fn audio_part(audio: Audio, kind: StemKind) -> Part {
    let sr = audio.sr as f64;
    let frames = audio.frames();
    let m = (frames as f64 / sr * SHAPE_HZ) as usize;
    let mut power = vec![0.0f64; m];
    for (f, p) in power.iter_mut().enumerate() {
        let a = (f as f64 / SHAPE_HZ * sr) as usize;
        let b = (((f + 1) as f64 / SHAPE_HZ * sr) as usize).min(frames);
        let s: f64 = (a..b).map(|i| (audio.mono(i) as f64).powi(2)).sum();
        *p = s / (b - a).max(1) as f64;
    }
    let db: Vec<f64> = (0..m)
        .map(|i| {
            let (lo, hi) = (i.saturating_sub(2), (i + 3).min(m));
            10.0 * (power[lo..hi].iter().sum::<f64>() / (hi - lo) as f64 + 1e-12).log10()
        })
        .collect();
    let mut sorted = db.clone();
    sorted.sort_by(f64::total_cmp);
    let loud = sorted.get(sorted.len() * 95 / 100).copied().unwrap_or(-120.0);
    let on: Vec<f32> = db.iter().map(|d| ((d - (loud - ON_RANGE)) / ON_RAMP).clamp(0.0, 1.0) as f32).collect();

    let chroma = chroma_range(kind).map(|(lo, hi)| {
        // Long enough to resolve a semitone near the bottom of a bass.
        let n = if sr >= 32_000.0 {
            8192
        } else if sr >= 16_000.0 {
            4096
        } else {
            2048
        };
        let fft = Fft::new(n);
        let window: Vec<f64> =
            (0..n).map(|i| 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos()).collect();
        let bins: Vec<(usize, usize)> = (1..n / 2)
            .filter_map(|k| {
                let f = k as f64 * sr / n as f64;
                (f >= lo && f <= hi).then(|| (k, ((12.0 * (f / 440.0).log2() + 69.0).round() as i64).rem_euclid(12) as usize))
            })
            .collect();
        let mut out = vec![[0.0f32; 12]; m];
        let mut re = vec![0.0f64; n];
        let mut im = vec![0.0f64; n];
        for f in 0..m {
            if on[f] < 0.05 {
                continue;
            }
            let start = ((f as f64 + 0.5) / SHAPE_HZ * sr) as isize - n as isize / 2;
            for i in 0..n {
                let j = start + i as isize;
                let s = if j >= 0 && (j as usize) < frames { audio.mono(j as usize) as f64 } else { 0.0 };
                re[i] = s * window[i];
                im[i] = 0.0;
            }
            fft.run(&mut re, &mut im, false);
            for (k, pc) in &bins {
                out[f][*pc] += (re[*k] * re[*k] + im[*k] * im[*k]).sqrt() as f32;
            }
        }
        smooth_chroma(&mut out, 2);
        for (f, c) in out.iter_mut().enumerate() {
            if on[f] < 0.05 {
                *c = [0.0; 12];
            } else {
                norm_chroma(c);
            }
        }
        out
    });
    Part { on, chroma }
}

/// What one tab frame against one recording frame costs: how far apart the parts are on whether
/// they are playing, plus, where both are, how far apart their notes are.
fn frame_cost(pairs: &[(&Part, &Part, f32)], total: f32, i: usize, j: usize) -> f32 {
    let mut c = 0.0f32;
    for (t, r, w) in pairs {
        let ot = t.on.get(i).copied().unwrap_or(0.0);
        let or = r.on.get(j).copied().unwrap_or(0.0);
        let mut x = (ot - or).abs();
        if let (Some(tc), Some(rc)) = (&t.chroma, &r.chroma) {
            let both = ot.min(or);
            if both > 0.0 {
                let (a, b) = (tc[i], rc.get(j).copied().unwrap_or([0.0; 12]));
                let dot: f32 = (0..12).map(|k| a[k] * b[k]).sum();
                x += CHROMA_WEIGHT * both * (1.0 - dot);
            }
        }
        c += w * x;
    }
    c / total
}

/// Match the whole tab against the whole recording by what each part is doing, and return the
/// map from tab seconds to recording seconds. Both ends are free: the recording may have any
/// amount of lead-in or tail, and the tab may start before the recording does or run past its
/// end.
///
/// The map is a tempo path: every `KNOT` frames of tab it moves on through the recording at some
/// pace, and changing pace costs `TEMPO_CHANGE` per step, except where the tab changes tempo
/// itself (`free` tab frames). A band keeps time, so a stretch where the music repeats and gives
/// nothing away is crossed at a steady pace, carrying what the stretches either side of it
/// settled - rather than wandering a bar or two to chase noise.
///
/// Pairs are `(tab part, recording part, weight)`. Also returns the mean cost per frame.
fn structure(
    pairs: &[(&Part, &Part, f32)],
    n: usize,
    m: usize,
    free: &[usize],
) -> Option<(Vec<(f64, f64)>, f32)> {
    use rayon::prelude::*;
    let total: f32 = pairs.iter().map(|p| p.2).sum();
    if n < 4 || m < 4 || pairs.is_empty() || total <= 0.0 {
        return None;
    }
    let mut cost = vec![0.0f32; n * m];
    cost.par_chunks_mut(m).enumerate().for_each(|(i, row)| {
        for (j, c) in row.iter_mut().enumerate() {
            *c = frame_cost(pairs, total, i, j);
        }
    });

    // Knots: every `KNOT` frames, and wherever the tab changes tempo.
    let mut marks: Vec<usize> = free.iter().copied().filter(|f| *f > 0 && *f < n).collect();
    marks.push(n);
    marks.sort_unstable();
    marks.dedup();
    let mut knots = vec![0usize];
    let mut loose = vec![true];
    for &b in &marks {
        let a = *knots.last().unwrap();
        if b <= a {
            continue;
        }
        let parts = (b - a).div_ceil(KNOT);
        for p in 1..=parts {
            knots.push(a + (b - a) * p / parts);
            loose.push(p == parts && b < n);
        }
    }

    let steps: Vec<usize> = (STEP_MIN..=STEP_MAX).collect();
    let ns = steps.len();
    let slack = TAB_SLACK as isize;
    let g_len = m + 2 * TAB_SLACK;
    // Where in the recording each tab frame of a segment lands, relative to its start.
    let offs: Vec<Vec<isize>> = steps
        .iter()
        .map(|&s| (0..KNOT).map(|r| ((s as f64 * (r as f64 + 0.5)) / KNOT as f64).floor() as isize).collect())
        .collect();
    let advance = |s: usize, len: usize| ((steps[s] * len) as f64 / KNOT as f64).round() as usize;

    let inf = f32::INFINITY;
    // Cost so far, indexed [step * g_len + g], where g is the recording frame plus `slack`.
    let mut cur = vec![0.0f32; ns * g_len];
    let mut back: Vec<Vec<u8>> = Vec::with_capacity(knots.len());
    back.push(Vec::new());
    for k in 0..knots.len() - 1 {
        let (i0, len) = (knots[k], knots[k + 1] - knots[k]);
        let change = if loose[k] { 0.0 } else { TEMPO_CHANGE };
        // Cheapest way into this knot at each recording frame for each next pace, with the cost
        // of changing pace to it: an L1 distance transform across paces, capped at a jump.
        let mut entry = vec![(inf, 0u8); ns * g_len];
        entry.par_chunks_mut(ns).enumerate().for_each(|(g, out)| {
            let mut low = (inf, 0u8);
            for s in 0..ns {
                let v = cur[s * g_len + g];
                if v < low.0 {
                    low = (v, s as u8);
                }
                let carried = if s > 0 { (out[s - 1].0 + change, out[s - 1].1) } else { (inf, 0) };
                out[s] = if v <= carried.0 { (v, s as u8) } else { carried };
            }
            for s in (0..ns.saturating_sub(1)).rev() {
                let carried = (out[s + 1].0 + change, out[s + 1].1);
                if carried.0 < out[s].0 {
                    out[s] = carried;
                }
            }
            for o in out.iter_mut() {
                if low.0 + TEMPO_JUMP < o.0 {
                    *o = (low.0 + TEMPO_JUMP, low.1);
                }
            }
        });
        // Then the segment itself, one pace at a time.
        let mut next = vec![inf; ns * g_len];
        let mut bp = vec![0u8; ns * g_len];
        next.par_chunks_mut(g_len).zip(bp.par_chunks_mut(g_len)).enumerate().for_each(|(s, (nx, b))| {
            let mut seg = vec![0.0f32; g_len];
            for r in 0..len {
                let row = &cost[(i0 + r).min(n - 1) * m..][..m];
                let o = offs[s][r.min(KNOT - 1)] - slack;
                for (g, acc) in seg.iter_mut().enumerate() {
                    let j = g as isize + o;
                    *acc += if j >= 0 && (j as usize) < m { row[j as usize] } else { SKIP };
                }
            }
            let adv = advance(s, len);
            for g2 in adv..g_len {
                let g = g2 - adv;
                let (v, from) = entry[g * ns + s];
                if v.is_finite() {
                    nx[g2] = v + seg[g];
                    b[g2] = from;
                }
            }
        });
        cur = next;
        back.push(bp);
    }

    // Free end, then follow the choices back.
    let last = knots.len() - 1;
    let (mut best, mut at) = (inf, 0usize);
    for (idx, v) in cur.iter().enumerate() {
        if *v < best {
            (best, at) = (*v, idx);
        }
    }
    if !best.is_finite() {
        return None;
    }
    let (mut s, mut g) = (at / g_len, at % g_len);
    let mut map = vec![(0.0, 0.0); knots.len()];
    for k in (0..=last).rev() {
        map[k] = (knots[k] as f64 / SHAPE_HZ, (g as isize - slack) as f64 / SHAPE_HZ);
        if k == 0 {
            break;
        }
        let prev = back[k][s * g_len + g] as usize;
        g -= advance(s, knots[k] - knots[k - 1]);
        s = prev;
    }
    Some((map, best / n as f32))
}

/// Follow a piecewise-linear map, continuing at `slope` beyond its ends. With `flip` the map is
/// read the other way round, from its second coordinate to its first; both must be increasing.
fn interp(map: &[(f64, f64)], x: f64, slope: f64, flip: bool) -> f64 {
    let p = |i: usize| if flip { (map[i].1, map[i].0) } else { map[i] };
    let n = map.len();
    if n == 0 {
        return x * slope;
    }
    let (x0, y0) = p(0);
    if x <= x0 || n == 1 {
        return y0 + (x - x0) * slope;
    }
    let (xn, yn) = p(n - 1);
    if x >= xn {
        return yn + (x - xn) * slope;
    }
    let (mut lo, mut hi) = (0, n - 1);
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if p(mid).0 <= x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let ((a, b), (c, d)) = (p(lo), p(hi));
    if c <= a { b } else { b + (d - b) * (x - a) / (c - a) }
}

// ---------------------------------------------------------------------------
// Alignment
// ---------------------------------------------------------------------------

/// What one stem had to say about the alignment.
#[derive(Clone, Copy, Debug)]
pub struct StemFit {
    /// Mean correlation between this stem's onsets and its part of the tab, 0..1.
    pub confidence: f32,
    /// Median distance from a tab hit to the recording's nearest hit, in seconds. NaN when
    /// there was nothing to compare.
    pub error: f64,
    /// Share of the tab's hits for this part that found a hit in the recording.
    pub found: f32,
}

impl Default for StemFit {
    fn default() -> Self {
        StemFit { confidence: 0.0, error: f64::NAN, found: 0.0 }
    }
}

/// Work out where a set of stems sits against the score and verify it all the way through.
/// Also returns what each stem on its own made of it, which is what the dialog lists.
pub fn align(sched: &Schedule, stems: &[(StemKind, Audio)]) -> (Alignment, Vec<StemFit>) {
    use rayon::prelude::*;
    let tl = &sched.timeline;
    let score_dur = tl.duration();
    let audio_dur = stems.iter().map(|(_, a)| a.frames() as f64 / a.sr as f64).fold(0.0, f64::max);
    let debug = std::env::var_os("GPT_ALIGN_DEBUG").is_some();

    let mut a = Alignment {
        offset: 0.0,
        rate: 1.0,
        confidence: 0.0,
        hit_error: f64::NAN,
        hit_part: String::new(),
        checks: Vec::new(),
        max_drift: 0.0,
        status: AlignStatus::Failed,
        summary: String::new(),
        detail: String::new(),
        map: Vec::new(),
    };
    let mut per_stem = vec![StemFit::default(); stems.len()];

    // Each stem's onsets and coarse shape, paired with the tab's onsets for the part it holds.
    let (recs, parts): (Vec<Vec<f32>>, Vec<Part>) =
        stems.par_iter().map(|(kind, audio)| (audio_onsets(*audio), audio_part(*audio, *kind))).unzip();
    let envs: Vec<(StemKind, Vec<f32>, Vec<f32>)> =
        stems.iter().zip(recs).map(|((kind, _), rec)| (*kind, rec, score_onsets(sched, *kind))).collect();
    if envs.iter().all(|(_, r, s)| r.len() < 200 || s.len() < 200) {
        a.summary = "The tab or the recording is too short to line up.".into();
        a.detail = a.summary.clone();
        return (a, per_stem);
    }
    // Vocals have nothing to match in a tab, so they only vote when nothing else can.
    let any_alignable = envs.iter().any(|(k, _, _)| k.alignable());
    let weight = |k: StemKind| {
        if !any_alignable || k.alignable() { 1.0f32 } else { 0.0 }
    };

    // Step 1: structure. Match the whole song at once by what each part is doing - which parts
    // are playing, and which notes - rather than by individual hits. A riff that repeats every
    // two bars makes the hits ambiguous by two bars, but the song around it is not: the drums
    // come in at one particular bar, the chords move at another. Nothing is assumed about where
    // the tab starts or ends in the recording, and the recording's tempo may wander section by
    // section; the tab is bent to follow it.
    let mix_part = tab_part(sched, StemKind::Mix);
    let tab_parts: Vec<Option<Part>> =
        envs.iter().map(|(k, _, _)| if k.alignable() { tab_part(sched, *k) } else { None }).collect();
    let mut pairs: Vec<(&Part, &Part, f32)> = Vec::new();
    for (i, (k, _, _)) in envs.iter().enumerate() {
        let w = weight(*k);
        let tab = if k.alignable() { tab_parts[i].as_ref() } else { mix_part.as_ref() };
        if let (true, Some(t)) = (w > 0.0, tab) {
            pairs.push((t, &parts[i], w));
        }
    }
    // A bass stem against a tab with no bass part: compare it with the band as a whole instead.
    if pairs.is_empty() {
        if let Some(t) = mix_part.as_ref() {
            pairs.extend(parts.iter().map(|p| (t, p, 1.0)));
        }
    }
    let n = pairs.iter().map(|p| p.0.on.len()).max().unwrap_or(0);
    let m = pairs.iter().map(|p| p.1.on.len()).max().unwrap_or(0);
    let free: Vec<usize> =
        tl.tempo_map.iter().map(|(tick, _)| (tl.seconds_at(*tick as f64) * SHAPE_HZ).round() as usize).collect();
    let Some((coarse, cost)) = structure(&pairs, n, m, &free) else {
        a.summary = "Couldn't line the recording up with the tab.".into();
        a.detail = format!(
            "The recording is {} long and the tab is {} long, and there was too little in the tab \
             to compare the recording with.\n",
            clock(audio_dur),
            clock(score_dur)
        );
        return (a, per_stem);
    };
    let coarse_at = |t: f64| interp(&coarse, t, 1.0, false);
    if debug {
        eprintln!("[align] structure: mean cost {cost:.3}");
        let mut t = 0.0;
        while t < score_dur {
            eprintln!("[align]   tab {t:6.1}s -> {:7.2}s", coarse_at(t));
            t += 10.0;
        }
    }

    // Step 2: hits. Place the tab's hits where the structure says they fall, and slide each
    // stretch of the song a fraction of a beat to wherever the recording's hits actually are.
    // The search is narrow on purpose: the structure has already decided which bar is which,
    // and this only settles where in that bar the beat lands.
    let rec_len = envs.iter().map(|(_, r, _)| r.len()).max().unwrap_or(0);
    let warped: Vec<(usize, f32, Vec<f32>)> = envs
        .par_iter()
        .enumerate()
        .filter(|(_, (k, _, _))| weight(*k) > 0.0)
        .map(|(i, (k, rec, _))| (i, weight(*k), score_onsets_at(sched, *k, rec.len(), &coarse_at)))
        .collect();
    let win = ((REFINE_WINDOW * HZ) as usize).min(rec_len);
    let hop = ((REFINE_HOP * HZ) as usize).max(1);
    let lo = ((coarse_at(0.0) * HZ).max(0.0) as usize).min(rec_len - win);
    let hi = ((coarse_at(score_dur) * HZ).max(0.0) as usize).clamp(lo + win, rec_len);
    let mut starts: Vec<usize> = (lo..=hi - win).step_by(hop).collect();
    if starts.last().is_some_and(|s| s + win < hi) {
        starts.push(hi - win);
    }

    let range = (REFINE_RANGE * HZ) as isize;
    let width = (2 * range + 1) as usize;
    let grid: Vec<(usize, Vec<f32>, Vec<(usize, Curve)>)> = starts
        .par_iter()
        .map(|&st| {
            let mut local = Vec::new();
            let mut per = Vec::new();
            for (si, w, env) in &warped {
                let Some(seg) = env.get(st..st + win) else { continue };
                // A window of rests has nothing to say about where the beat is.
                if seg.iter().filter(|v| **v > 0.0).count() < 4 {
                    continue;
                }
                let c = correlate_window(&envs[*si].1, seg, st as isize - range, st as isize + range);
                local.push((*w, c.clone()));
                per.push((*si, c));
            }
            // Clamp at zero: a negative correlation is evidence against that lag, not evidence
            // for any other, and letting it go negative lets a noisy window buy its way past
            // the smoothness penalty.
            let row = match combine(&local) {
                Some(c) => (-range..=range).map(|r| c.get(st as isize + r).unwrap_or(0.0).max(0.0)).collect(),
                None => vec![0.0; width],
            };
            (st, row, per)
        })
        .collect();

    // Pick the path through the windows rather than each window's own best guess. Neighbouring
    // windows overlap, so insisting the answer moves smoothly stops a window full of a repeated
    // riff from slipping a beat on its own.
    const SMOOTH: f32 = 0.004;
    const SLACK: isize = 2;
    let mut best = vec![vec![0.0f32; width]; grid.len()];
    let mut from = vec![vec![0usize; width]; grid.len()];
    for i in 0..grid.len() {
        for r in 0..width {
            let here = grid[i].1[r];
            if i == 0 {
                best[i][r] = here;
                continue;
            }
            let (mut pick, mut top) = (0usize, f32::MIN);
            for q in 0..width {
                let moved = ((r as isize - q as isize).abs() - SLACK).max(0);
                let v = best[i - 1][q] - SMOOTH * moved as f32;
                if v > top {
                    top = v;
                    pick = q;
                }
            }
            best[i][r] = here + top;
            from[i][r] = pick;
        }
    }
    let mut choice = vec![range as usize; grid.len()];
    if let Some(last) = best.last() {
        choice[grid.len() - 1] =
            last.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map(|(i, _)| i).unwrap_or(0);
        for i in (1..grid.len()).rev() {
            choice[i - 1] = from[i][choice[i]];
        }
    }

    // (recording time of the window centre, correction in seconds, confidence, confirmed)
    let mut fixes: Vec<(f64, f64, f32, bool)> = Vec::new();
    let mut stem_conf = vec![(0.0f64, 0usize); envs.len()];
    for (i, (st, row, per)) in grid.iter().enumerate() {
        let r = choice[i];
        let conf = row[r];
        let shift = (r as isize - range) as f64 + sub_frame(row, r);
        let centre = (st + win / 2) as f64 / HZ;
        let ok = conf >= CHECK_CONF;
        if ok {
            for (si, c) in per {
                stem_conf[*si].0 += c.get(*st as isize + r as isize - range).unwrap_or(0.0) as f64;
                stem_conf[*si].1 += 1;
            }
        }
        fixes.push((centre, shift / HZ, conf, ok));
    }
    // An unconfirmed window was quiet, not wrong, so it takes its correction from the confirmed
    // windows either side rather than from whatever its own noise suggested.
    let confirmed: Vec<(f64, f64)> = fixes.iter().filter(|f| f.3).map(|f| (f.0, f.1)).collect();
    let fix_at = |t: f64| interp(&confirmed, t, 0.0, false);

    // Step 3: the map playback follows. Every point is measured, so there is no single tempo for
    // it to drift away from; the recording sets the pace and the tab is bent to match.
    let mut map = Vec::new();
    let mut x = 0.0;
    while x < score_dur {
        let c = coarse_at(x);
        map.push((x, c + fix_at(c)));
        x += 0.25;
    }
    let c = coarse_at(score_dur);
    map.push((score_dur, c + fix_at(c)));
    a.map = tidy_map(map);
    let line: Vec<(f64, f64, f32)> = a.map.iter().map(|(x, y)| (*x, *y, 1.0)).collect();
    let (_, rate) = fit(&line);
    a.rate = if rate.is_finite() && rate > 0.0 { rate } else { 1.0 };
    a.offset = a.audio_time(0.0);

    // Step 4: report each window as a check on the structure match.
    let mut matched = 0;
    let mut total_conf = 0.0f64;
    for (centre, fix, conf, ok) in &fixes {
        let time = interp(&coarse, *centre, 1.0, true).clamp(0.0, score_dur);
        let bar = tl.locate(tl.tick_at_seconds(time) as u64).map(|k| tl.measures[k].index + 1).unwrap_or(0);
        if *ok {
            matched += 1;
            total_conf += *conf as f64;
            a.max_drift = a.max_drift.max(fix.abs());
        }
        a.checks.push(Check {
            time,
            bar,
            drift: if *ok { *fix } else { 0.0 },
            confidence: if *ok { *conf } else { 0.0 },
            matched: *ok,
        });
        if debug {
            eprintln!(
                "[align]   rec {centre:7.1}s (tab {time:6.1}s, bar {bar:3}) fix {:+.3}s corr {conf:.3}{}",
                fix,
                if *ok { "" } else { "  (unconfirmed)" }
            );
        }
    }
    if matched > 0 {
        a.confidence = (total_conf / matched as f64) as f32;
    }
    for (i, (sum, n)) in stem_conf.iter().enumerate() {
        per_stem[i].confidence = if *n > 0 { (sum / *n as f64) as f32 } else { 0.0 };
    }

    // Step 5: with the map settled, ask the blunt question - do the hits land together? A
    // correlation says how alike two signals look; this says how late the band is, in
    // milliseconds, which is what a player actually hears.
    for (i, (_, rec, score)) in envs.iter().enumerate() {
        let (err, found) = hit_error(&onset_peaks(rec), &onset_peaks(score), |t| a.audio_time(t));
        per_stem[i].error = err;
        per_stem[i].found = found;
    }
    // The headline comes from whichever part locates a beat most sharply, not an average of all
    // of them. A kick drum's attack is over in a few milliseconds, so the distance measured
    // against drums really is the alignment's error. A bass note or a chord swells in over 50ms
    // or more, so most of its measured distance is the softness of its own attack - averaging
    // that in would blame the alignment for something it did not do.
    let sharpest = envs
        .iter()
        .enumerate()
        .filter(|(i, (k, _, _))| k.alignable() && per_stem[*i].error.is_finite() && per_stem[*i].found >= 0.2)
        .min_by(|x, y| per_stem[x.0].error.total_cmp(&per_stem[y.0].error));
    if let Some((i, (k, _, _))) = sharpest {
        a.hit_error = per_stem[i].error;
        a.hit_part = k.label().to_string();
    }
    if debug {
        for (i, (kind, _, _)) in envs.iter().enumerate() {
            eprintln!(
                "[align] {:<9} agrees {:.0}%  hits land within {:.0}ms ({:.0}% of them found)",
                kind.label(),
                per_stem[i].confidence * 100.0,
                per_stem[i].error * 1000.0,
                per_stem[i].found * 100.0
            );
        }
        eprintln!("[align] overall hit error {:.0}ms", a.hit_error * 1000.0);
    }

    describe(&mut a, matched, audio_dur, score_dur, tl.bpm_at(0));
    (a, per_stem)
}

/// Turn the measurements into something a human can act on.
fn describe(a: &mut Alignment, matched: usize, audio_dur: f64, score_dur: f64, score_bpm: f64) {
    let total = a.checks.len();
    // The lead-in and tail are stated as findings, not assumptions: the fit is free to put bar 1
    // anywhere in the recording, so a count-in or extra intro bars simply show up here.
    let tail = audio_dur - a.audio_time(score_dur);
    let (lo, hi) = a.tempo_range();
    let mut d = format!(
        "Recording {}, tab {}.\n\
         Bar 1 of the tab starts {:.2}s into the recording, leaving {:.1}s of lead-in before the \
         tab begins and {:.1}s of recording after it ends. Neither end is assumed to match; both \
         fall out of the fit.\n\
         Tempo: the recording averages {:.1} bpm against the {:.0} bpm written at the top of the \
         tab ({:+.1}%); section by section it runs from {:+.1}% to {:+.1}% of the tab's tempo. The \
         recording keeps its own time and is never sped up or slowed down - the tab is stretched \
         to follow it.\n\
         {total} checks through the song, {matched} of them confirmed, match strength {:.0}%.\n",
        clock(audio_dur),
        clock(score_dur),
        a.offset,
        a.offset.max(0.0),
        tail.max(0.0),
        score_bpm / a.rate,
        score_bpm,
        a.tempo_pct(),
        lo,
        hi,
        a.confidence * 100.0,
    );
    if a.hit_error.is_finite() {
        d.push_str(&format!(
            "Timing: measured against {}, the part with the sharpest attack and so the one that can \
             pin down a beat, the tab's hits land a median {:.0}ms from the recording's. Under about \
             40ms the two sound like one performance. Parts that swell in rather than strike - bass, \
             held chords, voices - measure worse because their own attacks are soft, not because \
             they are lined up any differently; every part shares one alignment.\n",
            a.hit_part.to_lowercase(),
            a.hit_error * 1000.0
        ));
    }

    if a.confidence < GLOBAL_CONF || matched < 2 || matched * 3 < total {
        a.status = AlignStatus::Failed;
        a.summary = "Couldn't line this audio up with the tab.".into();
        d.push_str(
            "\nToo little of the recording matches the tab. That usually means it is a different \
             song, or a different arrangement: an extra intro, a section repeated a different \
             number of times, or a live take. Playing it would drift out of time, so it is left \
             switched off.\n",
        );
    } else {
        // An unconfirmed check is a quiet stretch, not a mistake, so it is not on its own a
        // reason to warn. What settles it is whether the hits land together: if they do, the
        // alignment has been proved by the thing the player will actually hear.
        let tight = a.hit_error.is_finite() && a.hit_error <= GOOD_HIT;
        a.status = if matched == total || tight { AlignStatus::Good } else { AlignStatus::Warn };
        a.summary = match (a.status, a.hit_error.is_finite()) {
            (AlignStatus::Good, true) => {
                format!("Lined up to within {:.0}ms: bar 1 starts {:.2}s in.", a.hit_error * 1000.0, a.offset)
            }
            (AlignStatus::Good, false) => format!("Lined up: bar 1 starts {:.2}s in.", a.offset),
            _ => format!("Lined up, {} of {total} checks unconfirmed.", total - matched),
        };
        if matched == total {
            d.push_str("\nEvery check through the song confirmed the alignment.\n");
        } else {
            d.push_str(&format!(
                "\n{} checks could not be confirmed. That is normal where the tab is mostly rests or \
                 one held note, and those stretches are carried across from the checks either side.{}\n",
                total - matched,
                if tight {
                    " The hits still land together through the song, which is the stronger test of the two."
                } else {
                    ""
                }
            ));
        }
    }

    // A check's drift is how far the hits pulled the tab from where the structure match put it:
    // small, beat-sized nudges are the refinement doing its job.
    if a.max_drift > DRIFT_BAD {
        let worst: Vec<&Check> = {
            let mut v: Vec<&Check> = a.checks.iter().filter(|c| c.matched).collect();
            v.sort_by(|x, y| y.drift.abs().total_cmp(&x.drift.abs()));
            v.into_iter().take(4).collect()
        };
        let bars: Vec<String> = worst.iter().map(|c| format!("bar {} ({:+.2}s)", c.bar, c.drift)).collect();
        d.push_str(&format!(
            "\nLining up individual hits moved the tab by up to {:.2}s from where the song's \
             structure first put it, most at {}. Playback follows the corrected positions.\n",
            a.max_drift,
            bars.join(", ")
        ));
    } else if a.max_drift > DRIFT_WARN {
        d.push_str(&format!(
            "\nLining up individual hits moved the tab by no more than {:.2}s anywhere.\n",
            a.max_drift
        ));
    }
    a.detail = d;
}

/// `m:ss`.
pub fn clock(secs: f64) -> String {
    format!("{}:{:02}", (secs / 60.0) as u32, (secs % 60.0) as u32)
}

/// The times of the clear hits in an onset envelope: a local maximum that stands well above the
/// quiet either side of it. Correlation asks whether two signals have the same shape; these are
/// for asking the blunter and more useful question of whether a hit lands in the same place.
fn onset_peaks(env: &[f32]) -> Vec<f64> {
    // A hit has to stand above its own neighbourhood, so a loud chorus does not swamp a verse.
    const NEAR: usize = (1.5 * HZ) as usize;
    let mut out = Vec::new();
    for i in 1..env.len().saturating_sub(1) {
        let v = env[i];
        if v <= 0.0 || v < env[i - 1] || v < env[i + 1] {
            continue;
        }
        let lo = i.saturating_sub(NEAR);
        let hi = (i + NEAR).min(env.len());
        let local: f32 = env[lo..hi].iter().sum::<f32>() / (hi - lo) as f32;
        if v > local * 3.0 {
            out.push(i as f64 / HZ);
        }
    }
    out
}

/// How far the tab's hits land from the recording's, once the map has been applied. This is the
/// number that says whether playing along will feel right, and unlike a correlation it is in
/// milliseconds, so it can be judged against what an ear will accept - about 20ms before a hit
/// sounds flammed, 50ms before it sounds wrong.
///
/// Returns the median error in seconds and the share of the tab's hits that found a partner.
fn hit_error(rec: &[f64], score: &[f64], map: impl Fn(f64) -> f64) -> (f64, f32) {
    // Wider than this and it is a different hit, not the same one played late.
    const REACH: f64 = 0.25;
    if rec.is_empty() || score.is_empty() {
        return (f64::NAN, 0.0);
    }
    let mut errs: Vec<f64> = Vec::new();
    let mut at = 0usize;
    for t in score {
        let want = map(*t);
        while at + 1 < rec.len() && rec[at + 1] < want {
            at += 1;
        }
        let near = [at.saturating_sub(1), at, (at + 1).min(rec.len() - 1)]
            .into_iter()
            .map(|i| rec[i] - want)
            .min_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap_or(f64::NAN);
        if near.abs() <= REACH {
            errs.push(near);
        }
    }
    let found = errs.len() as f32 / score.len() as f32;
    if errs.is_empty() {
        return (f64::NAN, 0.0);
    }
    errs.sort_by(|a, b| a.abs().total_cmp(&b.abs()));
    (errs[errs.len() / 2].abs(), found)
}

/// Keep the map going strictly forwards, so it can be followed either way round: from the tab
/// to the recording when seeking, and from the recording to the tab while playing.
fn tidy_map(points: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    for (x, y) in points {
        let Some(&(px, py)) = out.last() else {
            out.push((x, y));
            continue;
        };
        let dx = x - px;
        if dx <= 0.0 {
            continue;
        }
        out.push((x, y.max(py + dx * 0.01)));
    }
    out
}

/// Weighted least-squares fit of `audio = offset + rate * score`.
fn fit(points: &[(f64, f64, f32)]) -> (f64, f64) {
    let (mut sw, mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (x, y, c) in points {
        let w = *c as f64;
        sw += w;
        sx += w * x;
        sy += w * y;
        sxx += w * x * x;
        sxy += w * x * y;
    }
    let den = sw * sxx - sx * sx;
    if den.abs() < 1e-9 {
        return (if sw > 0.0 { sy / sw } else { 0.0 }, 1.0);
    }
    let rate = (sw * sxy - sx * sy) / den;
    ((sy - rate * sx) / sw, rate)
}

// ---------------------------------------------------------------------------
// FFT
// ---------------------------------------------------------------------------

/// Iterative radix-2 FFT with a precomputed twiddle table.
struct Fft {
    n: usize,
    tw_re: Vec<f64>,
    tw_im: Vec<f64>,
}

impl Fft {
    fn new(n: usize) -> Fft {
        assert!(n.is_power_of_two());
        let mut tw_re = Vec::with_capacity(n / 2);
        let mut tw_im = Vec::with_capacity(n / 2);
        for k in 0..n / 2 {
            let ang = -std::f64::consts::TAU * k as f64 / n as f64;
            tw_re.push(ang.cos());
            tw_im.push(ang.sin());
        }
        Fft { n, tw_re, tw_im }
    }

    fn run(&self, re: &mut [f64], im: &mut [f64], inverse: bool) {
        let n = self.n;
        debug_assert_eq!(re.len(), n);
        let mut j = 0usize;
        for i in 1..n {
            let mut bit = n >> 1;
            while j & bit != 0 {
                j ^= bit;
                bit >>= 1;
            }
            j |= bit;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2usize;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            let mut base = 0usize;
            while base < n {
                for k in 0..half {
                    let wr = self.tw_re[k * step];
                    let wi = if inverse { -self.tw_im[k * step] } else { self.tw_im[k * step] };
                    let (xr, xi) = (re[base + k + half], im[base + k + half]);
                    let (vr, vi) = (xr * wr - xi * wi, xr * wi + xi * wr);
                    let (ur, ui) = (re[base + k], im[base + k]);
                    re[base + k] = ur + vr;
                    im[base + k] = ui + vi;
                    re[base + k + half] = ur - vr;
                    im[base + k + half] = ui - vi;
                }
                base += len;
            }
            len <<= 1;
        }
        if inverse {
            let s = 1.0 / n as f64;
            for v in re.iter_mut() {
                *v *= s;
            }
            for v in im.iter_mut() {
                *v *= s;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TPQ, ToneKind};
    use crate::timeline::{NoteEvent, Schedule, Timeline, TrackInfo};

    #[test]
    fn fft_round_trips() {
        let fft = Fft::new(16);
        let mut re: Vec<f64> = (0..16).map(|i| (i as f64 * 0.37).sin()).collect();
        let mut im = vec![0.0; 16];
        let orig = re.clone();
        fft.run(&mut re, &mut im, false);
        fft.run(&mut re, &mut im, true);
        for (a, b) in orig.iter().zip(&re) {
            assert!((a - b).abs() < 1e-9, "{a} vs {b}");
        }
    }

    #[test]
    fn window_correlation_is_sub_frame_accurate() {
        let b: Vec<f32> = (0..200).map(|i| if i % 20 == 0 { 1.0 } else { 0.0 }).collect();
        let mut a = vec![0.0f32; 600];
        a[75..275].copy_from_slice(&b);
        let (lag, conf) = correlate_window(&a, &b, 50, 100).peak().unwrap();
        assert!((lag - 75.0).abs() < 0.2, "{lag}");
        assert!(conf > 0.9, "{conf}");
    }

    #[test]
    fn stems_vote_on_a_shared_lag() {
        // Two sources that each match at the true lag, and one that also matches elsewhere.
        let truth: Vec<f32> = (0..300).map(|i| if i % 11 == 0 { 1.0 } else { -0.09 }).collect();
        let mut a1 = vec![-0.09f32; 900];
        a1[200..500].copy_from_slice(&truth);
        let mut a2 = vec![-0.09f32; 900];
        a2[200..500].copy_from_slice(&truth);
        a2[600..900].copy_from_slice(&truth);
        let c1 = correlate_window(&a1, &truth, 0, 600);
        let c2 = correlate_window(&a2, &truth, 0, 600);
        let (lag, _) = combine(&[(1.0, c1), (1.0, c2)]).unwrap().peak().unwrap();
        assert!((lag - 200.0).abs() < 0.5, "{lag}");
    }

    /// Build a plain 4/4 song of `bars` bars at `bpm` with no tracks of its own.
    fn test_song(bars: usize, bpm: u32) -> crate::model::Song {
        use crate::model::*;
        let mut song = Song { tempo: bpm, ..Default::default() };
        for i in 0..bars {
            song.measure_headers.push(MeasureHeader {
                number: i + 1,
                start: (i as u32) * 4 * TPQ,
                numerator: 4,
                denominator: 4,
                repeat_close: -1,
                ..Default::default()
            });
        }
        song
    }

    /// Render one short percussive blip per event at `offset + rate * score_time`.
    fn render(
        sched: &Schedule,
        keep: impl Fn(&NoteEvent) -> bool,
        sr: u32,
        offset: f64,
        rate: f64,
        noisy: bool,
    ) -> Decoded {
        let dur = sched.timeline.duration() * rate + offset + 2.0;
        let hits: Vec<(f64, f32)> = sched
            .events
            .iter()
            .filter(|e| keep(e))
            .map(|e| (offset + rate * sched.timeline.seconds_at(e.start as f64), e.pitch))
            .collect();
        blips(&hits, sr, dur, noisy)
    }

    /// Short percussive blips at the given (seconds, pitch) points: noise bursts for drums,
    /// a sine at the note's pitch otherwise.
    fn blips(hits: &[(f64, f32)], sr: u32, dur: f64, noisy: bool) -> Decoded {
        let mut out = vec![0i16; (dur * sr as f64) as usize * 2];
        let mut seed = 12345u32;
        for &(at, pitch) in hits {
            let Some(start) = (at >= 0.0).then(|| (at * sr as f64) as usize) else { continue };
            let hz = if pitch > 0.0 { 440.0 * 2f32.powf((pitch - 69.0) / 12.0) } else { 110.0 };
            let len = sr as usize / 20;
            for i in 0..len {
                let env = (-(i as f32) / (sr as f32 * 0.01)).exp();
                let v = if noisy {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 8) as f32 / 8388608.0 - 1.0
                } else {
                    (i as f32 * std::f32::consts::TAU * hz / sr as f32).sin()
                };
                let s = (v * env * 8000.0) as i32;
                for ch in 0..2 {
                    let k = (start + i) * 2 + ch;
                    if k < out.len() {
                        out[k] = (out[k] as i32 + s).clamp(-32768, 32767) as i16;
                    }
                }
            }
        }
        Decoded { sr, samples: out }
    }

    /// The case that used to go wrong on real songs: a riff that repeats bar after bar, a
    /// recording with extra bars of that same riff before the tab starts, and a band that
    /// slows down for the second half. Matching hits alone could land any whole number of bars
    /// out; the bass coming in and the drums after it are what pin the bars down, and the
    /// tempo has to bend at the right place without the recording itself being stretched.
    #[test]
    fn align_survives_a_repeating_riff_and_extra_lead_in() {
        let bars = 40u64;
        let song = test_song(bars as usize, 120);
        let tl = Timeline::build(&song);
        let bar = 4 * TPQ as u64;
        let eighth = TPQ as u64 / 2;
        let riff = [40.0, 43.0, 45.0, 47.0, 45.0, 43.0, 52.0, 50.0];
        let mut events = Vec::new();
        for b in 0..bars {
            for (k, p) in riff.iter().enumerate() {
                let t = b * bar + k as u64 * eighth;
                events.push(NoteEvent { start: t, end: t + 200, track: 0, pitch: *p, velocity: 1.0, ..Default::default() });
            }
            // Bass from bar 17, a line that walks rather than loops.
            if b >= 16 {
                for q in 0..4u64 {
                    let t = b * bar + q * TPQ as u64;
                    let p = 28.0 + ((b * 5 + q * 3) % 12) as f32;
                    events.push(NoteEvent { start: t, end: t + 400, track: 1, pitch: p, velocity: 1.0, ..Default::default() });
                }
            }
            // Drums from bar 25.
            if b >= 24 {
                for e in 0..8u64 {
                    let t = b * bar + e * eighth;
                    events.push(NoteEvent { start: t, end: t + 100, track: 2, drum: true, velocity: 1.0, ..Default::default() });
                }
            }
        }
        let tracks = vec![
            TrackInfo { name: "Guitar".into(), tone: ToneKind::Clean, string_count: 6, pan: 0.0, volume: 1.0 },
            TrackInfo { name: "Bass".into(), tone: ToneKind::Bass, string_count: 4, pan: 0.0, volume: 1.0 },
            TrackInfo { name: "Drums".into(), tone: ToneKind::Drums, string_count: 6, pan: 0.0, volume: 1.0 },
        ];
        let sched = Schedule { events, clicks: vec![], timeline: tl, tracks };

        // One second of silence, six bars of the riff the tab doesn't have, then the tab: at
        // the written tempo up to bar 17 and 5% slower from there on.
        let lead = 1.0 + 6.0 * 2.0;
        let truth = |s: f64| if s < 32.0 { lead + s } else { lead + 32.0 + 1.05 * (s - 32.0) };
        let secs = |e: &NoteEvent| sched.timeline.seconds_at(e.start as f64);
        let dur = truth(sched.timeline.duration()) + 2.0;
        let sr = 11025;
        let mut guitar: Vec<(f64, f32)> = sched.events.iter().filter(|e| e.track == 0).map(|e| (truth(secs(e)), e.pitch)).collect();
        guitar.extend(sched.events.iter().filter(|e| e.track == 0 && secs(e) < 12.0).map(|e| (1.0 + secs(e), e.pitch)));
        let bass: Vec<(f64, f32)> = sched.events.iter().filter(|e| e.track == 1).map(|e| (truth(secs(e)), e.pitch)).collect();
        let drums: Vec<(f64, f32)> = sched.events.iter().filter(|e| e.drum).map(|e| (truth(secs(e)), 0.0)).collect();
        let (guitar, bass, drums) = (blips(&guitar, sr, dur, false), blips(&bass, sr, dur, false), blips(&drums, sr, dur, true));
        let (a, _) = align(
            &sched,
            &[(StemKind::Guitar, guitar.view()), (StemKind::Bass, bass.view()), (StemKind::Drums, drums.view())],
        );

        assert!(a.status != AlignStatus::Failed, "{}", a.detail);
        assert!((a.offset - lead).abs() < 0.1, "bar 1 at {:.2}s, want {lead}: {}", a.offset, a.detail);
        for s in [4.0, 16.0, 28.0, 36.0, 50.0, 64.0, 76.0] {
            let (got, want) = (a.audio_time(s), truth(s));
            assert!((got - want).abs() < 0.08, "tab {s}s plays at {got:.2}s, want {want:.2}s: {}", a.detail);
            // And back again, which is the way round Song mode uses it.
            assert!((a.score_time(want) - s).abs() < 0.08, "recording {want}s reads as tab {:.2}s", a.score_time(want));
        }
        let (lo, hi) = a.tempo_range();
        assert!(lo < -3.0 && hi > -1.5, "tempo range {lo:.1}%..{hi:.1}% should cover 0% and -4.8%");
    }

    /// The whole chain: synthesise stems from a score at a known offset and tempo, then
    /// check the matcher recovers both and that the warp map lands notes where they belong.
    #[test]
    fn align_recovers_a_known_offset_and_tempo() {
        let bars = 40;
        let song = test_song(bars, 120);
        let tl = Timeline::build(&song);
        let mut events = Vec::new();
        // A pattern that never repeats: a looping riff would let the matcher land a whole
        // loop away and still look right, which says nothing about the alignment.
        let mut seed = 987u32;
        let mut roll = || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 16) % 100
        };
        for bar in 0..bars as u64 {
            for eighth in 0..8u64 {
                let t = bar * 4 * TPQ as u64 + eighth * TPQ as u64 / 2;
                if roll() < 65 {
                    events.push(NoteEvent {
                        start: t,
                        end: t + 100,
                        track: 0,
                        drum: true,
                        velocity: 1.0,
                        ..Default::default()
                    });
                }
                if roll() < 45 {
                    events.push(NoteEvent {
                        start: t,
                        end: t + 400,
                        track: 1,
                        pitch: 40.0,
                        velocity: 1.0,
                        ..Default::default()
                    });
                }
            }
        }
        let tracks = vec![
            TrackInfo { name: "Drums".into(), tone: ToneKind::Drums, string_count: 6, pan: 0.0, volume: 1.0 },
            TrackInfo { name: "Bass".into(), tone: ToneKind::Bass, string_count: 4, pan: 0.0, volume: 1.0 },
        ];
        let sched = Schedule { events, clicks: vec![], timeline: tl, tracks };

        let (offset, rate) = (3.70, 1.012);
        let drums = render(&sched, |e| e.drum, 11025, offset, rate, true);
        let bass = render(&sched, |e| !e.drum, 11025, offset, rate, false);
        let (a, per_stem) = align(&sched, &[(StemKind::Drums, drums.view()), (StemKind::Bass, bass.view())]);

        assert!(a.status != AlignStatus::Failed, "{}", a.detail);
        // The line is only a scaffold for the map, so it gets the looser bound of the two.
        assert!((a.offset - offset).abs() < 0.12, "offset {} vs {offset}: {}", a.offset, a.detail);
        assert!((a.rate - rate).abs() < 0.004, "rate {} vs {rate}: {}", a.rate, a.detail);
        assert_eq!(per_stem.len(), 2);

        // The hit measure is what the user is shown, so hold it to the standard it claims: on a
        // rendering of the tab itself the hits should land all but on top of each other.
        let drums = per_stem[0];
        assert!(drums.found > 0.5, "only {:.0}% of the drum hits were found", drums.found * 100.0);
        assert!(drums.error < 0.03, "drum hits land {:.0}ms out", drums.error * 1000.0);
        assert!(a.hit_error < 0.03, "headline accuracy {:.0}ms", a.hit_error * 1000.0);
        assert!(!a.hit_part.is_empty(), "the accuracy figure should say which part it came from");

        // The map is what playback actually follows, so test that rather than the line.
        // Measurements sit at window centres, so the first and last few seconds of the song
        // are reached by extrapolation and get a slightly looser bound.
        for score_secs in [10.0, 20.0, 35.0, 50.0, 65.0, 72.0] {
            let want = offset + rate * score_secs;
            let got = a.audio_time(score_secs);
            assert!((got - want).abs() < 0.06, "at {score_secs}s: {got} vs {want}");
        }
        for score_secs in [0.0, 79.0] {
            let want = offset + rate * score_secs;
            let got = a.audio_time(score_secs);
            assert!((got - want).abs() < 0.09, "at {score_secs}s: {got} vs {want}");
        }
    }

    #[test]
    fn warp_map_stays_strictly_increasing() {
        // Playback inverts the map, so it has to keep moving forwards both ways: no backwards
        // steps, no repeated points, no flat stretches.
        let pts = vec![(0.0, 1.0), (10.0, 11.4), (10.0, 12.0), (20.0, 20.9), (30.0, 20.5), (40.0, 41.2)];
        let map = tidy_map(pts);
        assert_eq!(map.len(), 5, "{map:?}");
        assert!(map.windows(2).all(|w| w[1].0 > w[0].0 && w[1].1 > w[0].1), "{map:?}");
        assert_eq!(map[1], (10.0, 11.4));
        assert_eq!(map[4], (40.0, 41.2));
    }

    #[test]
    fn stem_kinds_come_from_file_names() {
        assert_eq!(StemKind::from_name("audio [drums].mp3"), StemKind::Drums);
        assert_eq!(StemKind::from_name("audio [bass].mp3"), StemKind::Bass);
        assert_eq!(StemKind::from_name("aduio-guitar-and-others.mp3"), StemKind::Guitar);
        assert_eq!(StemKind::from_name("song [vocals].mp3"), StemKind::Vocals);
        assert_eq!(StemKind::from_name("song (no_vocals).mp3"), StemKind::Mix);
        assert_eq!(StemKind::from_name("cantbringmedown.mp3"), StemKind::Mix);
    }
}
