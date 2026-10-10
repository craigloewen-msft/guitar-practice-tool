//! In-memory song model, loosely following the Guitar Pro structure.

/// Ticks per quarter note.
pub const TPQ: u32 = 960;

#[derive(Debug, Clone, Default)]
pub struct Song {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub tempo: u32,
    pub measure_headers: Vec<MeasureHeader>,
    pub tracks: Vec<Track>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TripletFeel {
    #[default]
    None,
    Eighth,
    Sixteenth,
}

#[derive(Debug, Clone, Default)]
pub struct MeasureHeader {
    /// 1-based measure number.
    pub number: usize,
    /// Start tick in score (non-repeated) time.
    pub start: u32,
    pub numerator: u8,
    pub denominator: u8,
    pub repeat_open: bool,
    /// Number of times to jump back (-1 = no repeat close).
    pub repeat_close: i32,
    /// Bitmask of the repeat passes in which this measure is played (0 = always).
    pub repeat_alternative: u8,
    pub marker: Option<String>,
    pub double_bar: bool,
    pub triplet_feel: TripletFeel,
}

impl MeasureHeader {
    pub fn length(&self) -> u32 {
        self.numerator as u32 * (TPQ * 4 / self.denominator.max(1) as u32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToneKind {
    Acoustic,
    Clean,
    Crunch,
    HighGain,
    /// Crunch / distortion from the DI guitar through the built-in amp simulator.
    CrunchAmp,
    HighGainAmp,
    /// DI guitar double-tracked through a Neural Amp Modeler capture.
    Nam,
    /// Picked bass.
    Bass,
    FingerBass,
    Drums,
}

impl ToneKind {
    pub const ALL_GUITAR: [ToneKind; 7] = [
        ToneKind::Nam,
        ToneKind::HighGain,
        ToneKind::Crunch,
        ToneKind::HighGainAmp,
        ToneKind::CrunchAmp,
        ToneKind::Clean,
        ToneKind::Acoustic,
    ];
    pub const ALL_BASS: [ToneKind; 2] = [ToneKind::Bass, ToneKind::FingerBass];

    pub fn label(self) -> &'static str {
        match self {
            ToneKind::Acoustic => "Acoustic",
            ToneKind::Clean => "Clean",
            ToneKind::Crunch => "Crunch",
            ToneKind::HighGain => "Distortion",
            ToneKind::CrunchAmp => "Crunch (amp sim)",
            ToneKind::HighGainAmp => "Distortion (amp sim)",
            ToneKind::Nam => "Metal (NAM amp)",
            ToneKind::Bass => "Bass (pick)",
            ToneKind::FingerBass => "Bass (finger)",
            ToneKind::Drums => "Drums",
        }
    }

    pub fn is_distorted(self) -> bool {
        matches!(self, ToneKind::Crunch | ToneKind::HighGain | ToneKind::CrunchAmp | ToneKind::HighGainAmp | ToneKind::Nam)
    }

    pub fn is_bass(self) -> bool {
        matches!(self, ToneKind::Bass | ToneKind::FingerBass)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Track {
    pub name: String,
    /// MIDI pitch per string; index 0 is string 1 (highest).
    pub strings: Vec<i32>,
    pub percussion: bool,
    /// General MIDI program.
    pub program: i32,
    /// 0..=127
    pub volume: u8,
    /// 0..=127, 64 = center
    pub balance: u8,
    /// Capo fret.
    pub capo: i32,
    pub color: (u8, u8, u8),
    pub measures: Vec<Measure>,
}

impl Track {
    /// Default tone, preferring the neural amp for distorted parts when a capture is installed.
    pub fn default_tone_with_nam(&self, nam_available: bool) -> ToneKind {
        match self.default_tone() {
            ToneKind::Crunch | ToneKind::HighGain if nam_available => ToneKind::Nam,
            t => t,
        }
    }

    pub fn default_tone(&self) -> ToneKind {
        if self.percussion {
            return ToneKind::Drums;
        }
        match self.program {
            24 | 25 => ToneKind::Acoustic,
            29 => ToneKind::Crunch,
            30 => ToneKind::HighGain,
            33 => ToneKind::FingerBass,
            32..=39 => ToneKind::Bass,
            _ => {
                if self.strings.len() <= 5 && self.strings.iter().all(|&p| p < 50) {
                    ToneKind::Bass
                } else {
                    ToneKind::Clean
                }
            }
        }
    }

    pub fn instrument_name(&self) -> &'static str {
        if self.percussion {
            return "Drums";
        }
        match self.program {
            24 => "Nylon Guitar",
            25 => "Acoustic Guitar",
            26 => "Jazz Guitar",
            27 => "Clean Guitar",
            28 => "Muted Guitar",
            29 => "Overdrive Guitar",
            30 => "Distortion Guitar",
            31 => "Guitar Harmonics",
            32 => "Acoustic Bass",
            33 => "Bass (finger)",
            34 => "Bass (pick)",
            35 => "Fretless Bass",
            36 | 37 => "Slap Bass",
            38 | 39 => "Synth Bass",
            _ => "Instrument",
        }
    }

    pub fn tuning_name(&self) -> String {
        let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
        self.strings
            .iter()
            .rev()
            .map(|&p| names[(p.rem_euclid(12)) as usize])
            .collect::<Vec<_>>()
            .join(" ")
    }
}

#[derive(Debug, Clone, Default)]
pub struct Measure {
    pub voices: Vec<Voice>,
}

#[derive(Debug, Clone, Default)]
pub struct Voice {
    pub beats: Vec<Beat>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BeatStatus {
    Empty,
    #[default]
    Normal,
    Rest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Duration {
    /// 1 = whole, 2 = half, 4 = quarter, ...
    pub value: u8,
    pub dotted: bool,
    pub double_dotted: bool,
    /// (enters, times), e.g. (3, 2) for triplets.
    pub tuplet: (u8, u8),
}

impl Default for Duration {
    fn default() -> Self {
        Duration { value: 4, dotted: false, double_dotted: false, tuplet: (1, 1) }
    }
}

impl Duration {
    pub fn ticks(&self) -> u32 {
        let mut t = TPQ * 4 / self.value.max(1) as u32;
        if self.dotted {
            t += t / 2;
        } else if self.double_dotted {
            t += t / 2 + t / 4;
        }
        if self.tuplet.0 > 1 {
            t = t * self.tuplet.1 as u32 / self.tuplet.0 as u32;
        }
        t
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrokeDir {
    Up,
    Down,
}

#[derive(Debug, Clone, Default)]
pub struct BeatEffects {
    pub wide_vibrato: bool,
    pub tremolo_bar: Option<Bend>,
    /// Direction and duration value of the strum.
    pub stroke: Option<(StrokeDir, u8)>,
    pub fade_in: bool,
    pub slap: u8,
}

#[derive(Debug, Clone, Default)]
pub struct Beat {
    /// Start tick in score time.
    pub start: u32,
    pub duration: Duration,
    pub status: BeatStatus,
    pub notes: Vec<Note>,
    pub text: Option<String>,
    pub effects: BeatEffects,
    pub tempo_change: Option<u32>,
}

impl Beat {
    pub fn ticks(&self) -> u32 {
        if self.status == BeatStatus::Empty { 0 } else { self.duration.ticks() }
    }
    pub fn is_rest(&self) -> bool {
        self.status == BeatStatus::Rest || self.notes.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NoteKind {
    #[default]
    Normal,
    Tie,
    Dead,
}

#[derive(Debug, Clone, Default)]
pub struct Note {
    /// 1-based string number (1 = highest).
    pub string: u8,
    pub fret: i32,
    pub kind: NoteKind,
    /// MIDI-style velocity 1..=127.
    pub velocity: u8,
    pub ghost: bool,
    pub accent: bool,
    pub heavy_accent: bool,
    pub effects: NoteEffects,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slide {
    ShiftTo,
    LegatoTo,
    OutDown,
    OutUp,
    InFromBelow,
    InFromAbove,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Harmonic {
    Natural,
    /// Artificial harmonic, sounding `semitones` above the fretted note.
    Artificial(i32),
    /// Tapped harmonic at the given fret.
    Tapped(i32),
    Pinch,
    Semi,
}

impl Harmonic {
    pub fn short_label(&self) -> &'static str {
        match self {
            Harmonic::Natural => "N.H.",
            Harmonic::Artificial(_) => "A.H.",
            Harmonic::Tapped(_) => "T.H.",
            Harmonic::Pinch => "P.H.",
            Harmonic::Semi => "S.H.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraceTransition {
    None,
    Slide,
    Bend,
    Hammer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grace {
    pub fret: i32,
    pub velocity: u8,
    /// Duration value (16, 32, 64...).
    pub duration: u8,
    pub transition: GraceTransition,
    pub dead: bool,
    pub on_beat: bool,
}

/// Bend/whammy curve: (position 0..=1 within the note, pitch offset in semitones).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Bend {
    pub points: Vec<(f32, f32)>,
}

impl Bend {
    pub fn max_value(&self) -> f32 {
        self.points.iter().map(|p| p.1).fold(0.0, |a, b| if b.abs() > a.abs() { b } else { a })
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoteEffects {
    pub bend: Option<Bend>,
    /// Hammer-on / pull-off to the next note on this string.
    pub hammer: bool,
    pub let_ring: bool,
    pub staccato: bool,
    pub palm_mute: bool,
    pub vibrato: bool,
    pub grace: Option<Grace>,
    pub slides: Vec<Slide>,
    pub harmonic: Option<Harmonic>,
    /// (trill fret, period in ticks)
    pub trill: Option<(i32, u32)>,
    /// Tremolo picking period in ticks.
    pub tremolo_picking: Option<u32>,
}

impl NoteEffects {
    pub fn has_slide(&self, s: Slide) -> bool {
        self.slides.contains(&s)
    }
}
