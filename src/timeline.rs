//! Playback timeline (repeat expansion, tempo map) and conversion of the score into synth events.

use crate::model::*;

#[derive(Debug, Clone, Copy)]
pub struct PlayMeasure {
    /// Index into `song.measure_headers`.
    pub index: usize,
    /// Start tick in playback time.
    pub start: u64,
    pub len: u32,
}

#[derive(Debug, Clone, Default)]
pub struct Timeline {
    pub measures: Vec<PlayMeasure>,
    /// (playback tick, bpm), sorted, first entry at tick 0.
    pub tempo_map: Vec<(u64, f64)>,
    /// Seconds (at 100% speed) at each tempo map entry.
    tempo_secs: Vec<f64>,
    pub total: u64,
}

impl Timeline {
    pub fn build(song: &Song) -> Timeline {
        let headers = &song.measure_headers;
        let mut measures = Vec::new();
        let mut i = 0usize;
        let mut repeat_start = 0usize;
        let mut pass: u32 = 1;
        let mut counters = vec![0i32; headers.len()];
        let mut jumped_back = false;
        let mut tick = 0u64;
        let mut guard = 0usize;
        while i < headers.len() && guard < 100_000 {
            guard += 1;
            let h = &headers[i];
            if h.repeat_open && !jumped_back {
                repeat_start = i;
                pass = 1;
            }
            jumped_back = false;
            if h.repeat_alternative != 0 && pass <= 8 && h.repeat_alternative & (1 << (pass - 1)) == 0 {
                i += 1;
                continue;
            }
            measures.push(PlayMeasure { index: i, start: tick, len: h.length() });
            tick += h.length() as u64;
            if h.repeat_close > 0 {
                if counters[i] < h.repeat_close {
                    counters[i] += 1;
                    pass += 1;
                    i = repeat_start;
                    jumped_back = true;
                    continue;
                } else {
                    counters[i] = 0;
                    repeat_start = i + 1;
                    pass = 1;
                }
            }
            i += 1;
        }

        let mut tempo_map: Vec<(u64, f64)> = vec![(0, song.tempo.max(1) as f64)];
        for pm in &measures {
            let hstart = headers[pm.index].start;
            for track in &song.tracks {
                let Some(m) = track.measures.get(pm.index) else { continue };
                for v in &m.voices {
                    for b in &v.beats {
                        if let Some(t) = b.tempo_change {
                            tempo_map.push((pm.start + (b.start - hstart) as u64, t.max(1) as f64));
                        }
                    }
                }
            }
        }
        tempo_map.sort_by_key(|e| e.0);
        let mut dedup: Vec<(u64, f64)> = Vec::new();
        for e in tempo_map {
            if let Some(last) = dedup.last_mut() {
                if last.0 == e.0 {
                    *last = e;
                    continue;
                }
            }
            dedup.push(e);
        }
        let mut tempo_secs = Vec::with_capacity(dedup.len());
        let mut secs = 0.0;
        for (k, e) in dedup.iter().enumerate() {
            if k > 0 {
                let prev = dedup[k - 1];
                secs += (e.0 - prev.0) as f64 / TPQ as f64 * 60.0 / prev.1;
            }
            tempo_secs.push(secs);
        }
        Timeline { measures, tempo_map: dedup, tempo_secs, total: tick }
    }

    pub fn bpm_at(&self, tick: u64) -> f64 {
        let k = self.tempo_map.partition_point(|e| e.0 <= tick);
        self.tempo_map[k.saturating_sub(1)].1
    }

    /// Seconds (at 100% speed) at the given playback tick.
    pub fn seconds_at(&self, tick: f64) -> f64 {
        let k = self.tempo_map.partition_point(|e| (e.0 as f64) <= tick).saturating_sub(1);
        let (t0, bpm) = self.tempo_map[k];
        self.tempo_secs[k] + (tick - t0 as f64) / TPQ as f64 * 60.0 / bpm
    }

    /// Playback tick at the given time in seconds (inverse of `seconds_at`).
    pub fn tick_at_seconds(&self, secs: f64) -> f64 {
        if self.tempo_map.is_empty() {
            return 0.0;
        }
        let k = self.tempo_secs.partition_point(|s| *s <= secs).saturating_sub(1);
        let (t0, bpm) = self.tempo_map[k];
        t0 as f64 + (secs - self.tempo_secs[k]) * bpm / 60.0 * TPQ as f64
    }

    /// Total length of the playback timeline in seconds (at 100% speed).
    pub fn duration(&self) -> f64 {
        self.seconds_at(self.total as f64)
    }

    /// Index into `measures` of the play measure containing `tick`.
    pub fn locate(&self, tick: u64) -> Option<usize> {
        if self.measures.is_empty() {
            return None;
        }
        let k = self.measures.partition_point(|m| m.start <= tick);
        Some(k.saturating_sub(1))
    }

    /// First playback occurrence of a score measure at or after `from` tick, else the first one overall.
    pub fn occurrence(&self, measure_index: usize, near: u64) -> Option<&PlayMeasure> {
        let mut first = None;
        let mut best: Option<&PlayMeasure> = None;
        for pm in &self.measures {
            if pm.index != measure_index {
                continue;
            }
            if first.is_none() {
                first = Some(pm);
            }
            let cur_dist = (pm.start as i64 - near as i64).abs();
            if best.is_none_or(|b| cur_dist < (b.start as i64 - near as i64).abs()) {
                best = Some(pm);
            }
        }
        best.or(first)
    }
}

#[derive(Debug, Clone, Default)]
pub struct NoteEvent {
    pub start: u64,
    pub end: u64,
    pub track: usize,
    /// 0-based string index (0 = highest). For drums: unused.
    pub string: usize,
    /// MIDI pitch (fractional allowed). For drums: the GM drum key.
    pub pitch: f32,
    pub velocity: f32,
    pub palm_mute: bool,
    pub dead: bool,
    pub harmonic: bool,
    /// Don't re-pluck: hammer-on, pull-off, legato slide, trill.
    pub legato: bool,
    pub let_ring: bool,
    pub drum: bool,
    /// Pitch offset curve (ticks relative to start, semitones), piecewise linear; holds last value.
    pub curve: Vec<(u32, f32)>,
    /// Vibrato (start tick relative to note start, depth in semitones).
    pub vibrato: Option<(u32, f32)>,
    /// Fade the note out at the end (slide out).
    pub fade_out: bool,
}

impl NoteEvent {
    pub fn curve_at(&self, rel: f64) -> f32 {
        curve_eval(&self.curve, rel)
    }
}

pub fn curve_eval(curve: &[(u32, f32)], rel: f64) -> f32 {
    if curve.is_empty() {
        return 0.0;
    }
    if rel <= curve[0].0 as f64 {
        return curve[0].1;
    }
    for w in curve.windows(2) {
        let (t0, v0) = (w[0].0 as f64, w[0].1);
        let (t1, v1) = (w[1].0 as f64, w[1].1);
        if rel <= t1 {
            if t1 <= t0 {
                return v1;
            }
            return v0 + (v1 - v0) * ((rel - t0) / (t1 - t0)) as f32;
        }
    }
    curve[curve.len() - 1].1
}

/// Sum of two piecewise-linear curves.
fn curve_add(a: &[(u32, f32)], b: &[(u32, f32)]) -> Vec<(u32, f32)> {
    if a.is_empty() {
        return b.to_vec();
    }
    if b.is_empty() {
        return a.to_vec();
    }
    let mut ts: Vec<u32> = a.iter().chain(b.iter()).map(|p| p.0).collect();
    ts.sort_unstable();
    ts.dedup();
    ts.iter().map(|&t| (t, curve_eval(a, t as f64) + curve_eval(b, t as f64))).collect()
}

#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub name: String,
    pub tone: ToneKind,
    pub string_count: usize,
    /// -1..1
    pub pan: f32,
    /// 0..1
    pub volume: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Schedule {
    pub events: Vec<NoteEvent>,
    /// (tick, accented) metronome clicks.
    pub clicks: Vec<(u64, bool)>,
    pub timeline: Timeline,
    pub tracks: Vec<TrackInfo>,
}

fn harmonic_interval(fret: i32) -> f32 {
    match fret {
        0 => 12.0,
        2 => 34.0,
        3 => 31.0,
        4 | 9 | 16 => 28.0,
        5 | 24 => 24.0,
        7 | 19 => 19.0,
        12 => 12.0,
        _ => fret as f32 + 12.0,
    }
}

fn swing(rel: u32, feel: TripletFeel) -> u32 {
    let unit = match feel {
        TripletFeel::None => return rel,
        TripletFeel::Eighth => TPQ,
        TripletFeel::Sixteenth => TPQ / 2,
    };
    let base = rel / unit * unit;
    let r = rel - base;
    let half = unit / 2;
    let long = unit * 2 / 3;
    let mapped = if r < half { r * long / half } else { long + (r - half) * (unit - long) / half };
    base + mapped
}

#[derive(Clone, Copy, Default)]
struct StringState {
    last_event: Option<usize>,
    pending_legato: bool,
    pending_slide: bool,
}

pub fn build_schedule(song: &Song, tones: &[ToneKind]) -> Schedule {
    let timeline = Timeline::build(song);
    let headers = &song.measure_headers;
    let mut events: Vec<NoteEvent> = Vec::new();

    for (ti, track) in song.tracks.iter().enumerate() {
        let n_voices = track.measures.iter().map(|m| m.voices.len()).max().unwrap_or(0);
        for vi in 0..n_voices {
            let mut st = [StringState::default(); 8];
            for pm in &timeline.measures {
                let h = &headers[pm.index];
                let Some(voice) = track.measures.get(pm.index).and_then(|m| m.voices.get(vi)) else { continue };
                for beat in &voice.beats {
                    if beat.status != BeatStatus::Normal || beat.notes.is_empty() {
                        continue;
                    }
                    let rel = beat.start.saturating_sub(h.start);
                    let rel_end = rel + beat.ticks();
                    let b_start = pm.start + swing(rel, h.triplet_feel) as u64;
                    let b_end = pm.start + swing(rel_end, h.triplet_feel) as u64;
                    let dur = (b_end - b_start).max(1) as u32;
                    schedule_beat(track, ti, beat, b_start, dur, &mut st, &mut events);
                }
            }
        }
    }
    events.sort_by_key(|e| e.start);

    let mut clicks = Vec::new();
    for pm in &timeline.measures {
        let h = &headers[pm.index];
        let step = TPQ * 4 / h.denominator.max(1) as u32;
        for k in 0..h.numerator as u32 {
            clicks.push((pm.start + (k * step) as u64, k == 0));
        }
    }

    let tracks = song
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| TrackInfo {
            name: t.name.clone(),
            tone: tones.get(i).copied().unwrap_or_else(|| t.default_tone()),
            string_count: t.strings.len(),
            pan: ((t.balance as f32 - 64.0) / 64.0).clamp(-1.0, 1.0),
            volume: (t.volume as f32 / 127.0).clamp(0.0, 1.0),
        })
        .collect();

    Schedule { events, clicks, timeline, tracks }
}

fn schedule_beat(
    track: &Track,
    ti: usize,
    beat: &Beat,
    b_start: u64,
    dur: u32,
    st: &mut [StringState; 8],
    events: &mut Vec<NoteEvent>,
) {
    let n_notes = beat.notes.len();
    let stroke_ticks = beat
        .effects
        .stroke
        .map(|(_, v)| (TPQ * 4 / v.max(1) as u32).min(dur / 2))
        .unwrap_or(0);
    let tremolo_bar: Vec<(u32, f32)> = beat
        .effects
        .tremolo_bar
        .as_ref()
        .map(|b| b.points.iter().map(|&(p, v)| ((p * dur as f32) as u32, v)).collect())
        .unwrap_or_default();

    // Order notes for strums: down-strokes start on the low strings.
    let mut order: Vec<usize> = (0..n_notes).collect();
    match beat.effects.stroke {
        Some((StrokeDir::Down, _)) => order.sort_by_key(|&i| std::cmp::Reverse(beat.notes[i].string)),
        Some((StrokeDir::Up, _)) => order.sort_by_key(|&i| beat.notes[i].string),
        None => {}
    }

    for (k, &ni) in order.iter().enumerate() {
        let note = &beat.notes[ni];
        let s = (note.string as usize).saturating_sub(1);
        if s >= 8 {
            continue;
        }
        let fx = &note.effects;
        let mut vel = note.velocity as f32 / 127.0;
        if note.ghost {
            vel *= 0.55;
        }
        if note.accent {
            vel *= 1.15;
        }
        if note.heavy_accent {
            vel *= 1.3;
        }
        let vel = vel.clamp(0.05, 1.0);

        if track.percussion {
            events.push(NoteEvent {
                start: b_start,
                end: b_start + dur as u64,
                track: ti,
                string: s,
                pitch: note.fret as f32,
                velocity: vel,
                drum: true,
                ..Default::default()
            });
            continue;
        }

        let Some(&open) = track.strings.get(s) else { continue };
        let open = (open + track.capo) as f32;
        let fretted = open + note.fret as f32;
        let start = b_start + if n_notes > 1 { (stroke_ticks as u64 * k as u64) / n_notes as u64 } else { 0 };
        let vibrato_depth = if beat.effects.wide_vibrato {
            Some(0.6)
        } else if fx.vibrato {
            Some(0.3)
        } else {
            None
        };

        // Tied note: extend the previous event on this string.
        if note.kind == NoteKind::Tie {
            if let Some(ei) = st[s].last_event {
                let ev = &mut events[ei];
                let off = (b_start.saturating_sub(ev.start)) as u32;
                let new_end = if fx.let_ring || ev.let_ring {
                    b_start + (dur as u64).max(TPQ as u64 * 16)
                } else {
                    b_start + dur as u64
                };
                ev.end = ev.end.max(new_end);
                if let Some(b) = &fx.bend {
                    let held = curve_eval(&ev.curve, off as f64);
                    let mut c = ev.curve.clone();
                    if c.is_empty() || held != 0.0 {
                        c.push((off, held));
                    }
                    for &(p, v) in &b.points {
                        c.push((off + (p * dur as f32) as u32, v));
                    }
                    ev.curve = c;
                }
                if !tremolo_bar.is_empty() {
                    let shifted: Vec<(u32, f32)> = tremolo_bar.iter().map(|&(t, v)| (t + off, v)).collect();
                    ev.curve = curve_add(&ev.curve, &shifted);
                }
                if let Some(d) = vibrato_depth {
                    if ev.vibrato.is_none() {
                        ev.vibrato = Some((off, d));
                    }
                }
                let ev_len = (ev.end - ev.start) as u32;
                apply_slide_out(ev, fx, ev_len, dur);
                st[s].pending_legato = fx.hammer || fx.has_slide(Slide::LegatoTo);
                st[s].pending_slide = fx.has_slide(Slide::LegatoTo) || fx.has_slide(Slide::ShiftTo);
                continue;
            }
        }

        let pitch = match fx.harmonic {
            Some(Harmonic::Natural) => open + harmonic_interval(note.fret),
            Some(Harmonic::Artificial(n)) => fretted + n as f32,
            Some(Harmonic::Tapped(_)) | Some(Harmonic::Semi) => fretted + 12.0,
            Some(Harmonic::Pinch) => fretted + 19.0,
            None => fretted,
        };

        let mut legato = st[s].pending_legato;
        let mut start = start;

        // Grace note
        if let Some(g) = fx.grace {
            let gt = (TPQ * 4 / g.duration.max(1) as u32).min(dur / 2).max(1) as u64;
            let g_start = if g.on_beat { start } else { start.saturating_sub(gt) };
            if g.on_beat {
                start += gt;
            }
            let g_pitch = open + g.fret as f32;
            let mut gev = NoteEvent {
                start: g_start,
                end: g_start + gt,
                track: ti,
                string: s,
                pitch: g_pitch,
                velocity: g.velocity as f32 / 127.0,
                dead: g.dead,
                palm_mute: fx.palm_mute,
                legato,
                ..Default::default()
            };
            match g.transition {
                GraceTransition::Slide => {
                    gev.curve = vec![(0, 0.0), (gt as u32, pitch - g_pitch)];
                    gev.end = start;
                    legato = true;
                }
                GraceTransition::Hammer | GraceTransition::Bend => {
                    gev.end = start;
                    legato = true;
                }
                GraceTransition::None => {}
            }
            events.push(gev);
            st[s].last_event = Some(events.len() - 1);
        }

        // Slide into this note from the previous one on the same string.
        if st[s].pending_slide {
            if let Some(ei) = st[s].last_event {
                let ev = &mut events[ei];
                let len = (start.saturating_sub(ev.start)) as u32;
                if len > 0 && ev.end + (TPQ as u64) >= start {
                    let slide_len = (len / 2).min(TPQ / 2).max(1);
                    let t0 = len - slide_len;
                    let held = curve_eval(&ev.curve, t0 as f64);
                    let mut c: Vec<(u32, f32)> = ev.curve.iter().copied().filter(|p| p.0 < t0).collect();
                    c.push((t0, held));
                    c.push((len, held + pitch - ev.pitch));
                    ev.curve = c;
                    ev.end = ev.end.max(start);
                }
            }
        }

        let ring_end = start + (dur as u64).max(TPQ as u64 * 16);
        let end = if fx.let_ring {
            ring_end
        } else if fx.staccato {
            start + (dur / 2).max(1) as u64
        } else {
            start + dur as u64
        };

        let mut curve: Vec<(u32, f32)> = Vec::new();
        if let Some(b) = &fx.bend {
            curve = b.points.iter().map(|&(p, v)| ((p * dur as f32) as u32, v)).collect();
        }
        let slide_in = (dur / 4).min(TPQ / 4).max(1);
        if fx.has_slide(Slide::InFromBelow) {
            curve = curve_add(&curve, &[(0, -4.0), (slide_in, 0.0)]);
        }
        if fx.has_slide(Slide::InFromAbove) {
            curve = curve_add(&curve, &[(0, 4.0), (slide_in, 0.0)]);
        }
        if !tremolo_bar.is_empty() {
            curve = curve_add(&curve, &tremolo_bar);
        }

        let base = NoteEvent {
            start,
            end,
            track: ti,
            string: s,
            pitch,
            velocity: vel,
            palm_mute: fx.palm_mute,
            dead: note.kind == NoteKind::Dead,
            harmonic: fx.harmonic.is_some(),
            legato,
            let_ring: fx.let_ring,
            drum: false,
            curve,
            vibrato: vibrato_depth.map(|d| (0, d)),
            fade_out: false,
        };

        if let Some((trill_fret, period)) = fx.trill {
            let period = period.max(30) as u64;
            let alt = open + trill_fret as f32;
            let mut t = start;
            let mut k = 0;
            while t < start + dur as u64 {
                let mut e = base.clone();
                e.start = t;
                e.end = (t + period).min(start + dur as u64);
                e.pitch = if k % 2 == 0 { pitch } else { alt };
                e.legato = k > 0 || legato;
                e.curve.clear();
                events.push(e);
                t += period;
                k += 1;
            }
        } else if let Some(period) = fx.tremolo_picking {
            let period = period.max(30) as u64;
            let mut t = start;
            let mut k = 0;
            while t < start + dur as u64 {
                let mut e = base.clone();
                e.start = t;
                e.end = (t + period).min(start + dur as u64);
                e.legato = k == 0 && legato;
                events.push(e);
                t += period;
                k += 1;
            }
        } else {
            let mut e = base;
            apply_slide_out(&mut e, fx, dur, dur);
            events.push(e);
        }

        st[s].last_event = Some(events.len() - 1);
        st[s].pending_legato = fx.hammer || fx.has_slide(Slide::LegatoTo);
        st[s].pending_slide = fx.has_slide(Slide::LegatoTo) || fx.has_slide(Slide::ShiftTo);
    }
}

fn apply_slide_out(ev: &mut NoteEvent, fx: &NoteEffects, total_len: u32, last_dur: u32) {
    let dir = if fx.has_slide(Slide::OutDown) {
        -7.0
    } else if fx.has_slide(Slide::OutUp) {
        7.0
    } else {
        return;
    };
    let len = (last_dur / 2).min(TPQ / 2).max(1);
    let t0 = total_len.saturating_sub(len);
    let held = curve_eval(&ev.curve, t0 as f64);
    let mut c: Vec<(u32, f32)> = ev.curve.iter().copied().filter(|p| p.0 < t0).collect();
    c.push((t0, held));
    c.push((total_len, held + dir));
    ev.curve = c;
    ev.fade_out = true;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(n: usize, open: bool, close: i32, alt: u8) -> MeasureHeader {
        MeasureHeader {
            number: n,
            start: TPQ + (n as u32 - 1) * TPQ * 4,
            numerator: 4,
            denominator: 4,
            repeat_open: open,
            repeat_close: close,
            repeat_alternative: alt,
            ..Default::default()
        }
    }

    #[test]
    fn repeats_with_alternative_endings() {
        let song = Song {
            tempo: 120,
            measure_headers: vec![
                header(1, false, -1, 0),
                header(2, true, -1, 0),
                header(3, false, 1, 0b01),
                header(4, false, -1, 0b10),
                header(5, false, -1, 0),
            ],
            ..Default::default()
        };
        let tl = Timeline::build(&song);
        let order: Vec<usize> = tl.measures.iter().map(|m| m.index + 1).collect();
        assert_eq!(order, vec![1, 2, 3, 2, 4, 5]);
    }

    #[test]
    fn simple_repeat_x3() {
        let song = Song {
            tempo: 120,
            measure_headers: vec![header(1, true, -1, 0), header(2, false, 2, 0), header(3, false, -1, 0)],
            ..Default::default()
        };
        let tl = Timeline::build(&song);
        let order: Vec<usize> = tl.measures.iter().map(|m| m.index + 1).collect();
        assert_eq!(order, vec![1, 2, 1, 2, 1, 2, 3]);
        assert!((tl.seconds_at(TPQ as f64 * 4.0) - 2.0).abs() < 1e-9);
    }

    #[test]
    fn curves() {
        let c = vec![(0, 0.0), (100, 2.0)];
        assert_eq!(curve_eval(&c, 50.0), 1.0);
        assert_eq!(curve_eval(&c, 500.0), 2.0);
        let s = curve_add(&c, &[(50, 1.0)]);
        assert_eq!(curve_eval(&s, 100.0), 3.0);
    }
}
