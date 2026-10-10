//! Guitar Pro 3/4/5 (.gp3, .gp4, .gp5) binary reader.

use crate::model::*;
use anyhow::{Context, Result, bail};

const BEND_POSITION: f32 = 60.0;
/// Raw bend units per semitone (100 = full tone).
const BEND_SEMITONE: f32 = 50.0;

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    /// (major, minor, patch) e.g. (5, 1, 0) for v5.10
    version: (u8, u8, u8),
}

#[derive(Clone, Copy, Default)]
struct MidiChannel {
    instrument: i32,
    volume: u8,
    balance: u8,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<()> {
        if self.pos + n > self.data.len() {
            bail!("unexpected end of file at byte {}", self.pos);
        }
        Ok(())
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.need(n)?;
        self.pos += n;
        Ok(())
    }
    fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }
    fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
    fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }
    fn i16(&mut self) -> Result<i16> {
        self.need(2)?;
        let v = i16::from_le_bytes([self.data[self.pos], self.data[self.pos + 1]]);
        self.pos += 2;
        Ok(v)
    }
    fn i32(&mut self) -> Result<i32> {
        self.need(4)?;
        let b = &self.data[self.pos..self.pos + 4];
        self.pos += 4;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn f64(&mut self) -> Result<f64> {
        self.need(8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.data[self.pos..self.pos + 8]);
        self.pos += 8;
        Ok(f64::from_le_bytes(b))
    }
    fn decode(bytes: &[u8]) -> String {
        // Guitar Pro uses Windows-1252; latin-1 is close enough for display.
        bytes.iter().map(|&b| b as char).collect()
    }
    /// A string with a 1-byte length prefix stored in a fixed `count`-byte field.
    fn byte_size_string(&mut self, count: usize) -> Result<String> {
        let size = self.u8()? as usize;
        self.need(count)?;
        let s = Self::decode(&self.data[self.pos..self.pos + size.min(count)]);
        self.pos += count;
        Ok(s)
    }
    /// 4-byte total size followed by a byte-size string.
    fn int_byte_size_string(&mut self) -> Result<String> {
        let count = self.i32()?;
        if count <= 0 {
            return Ok(String::new());
        }
        self.byte_size_string(count as usize - 1)
    }
    fn int_size_string(&mut self) -> Result<String> {
        let count = self.i32()?.max(0) as usize;
        self.need(count)?;
        let s = Self::decode(&self.data[self.pos..self.pos + count]);
        self.pos += count;
        Ok(s)
    }

    fn gp3(&self) -> bool {
        self.version.0 == 3
    }
    fn gp4(&self) -> bool {
        self.version.0 == 4
    }
    fn gp5(&self) -> bool {
        self.version.0 == 5
    }
    fn gp500(&self) -> bool {
        self.version == (5, 0, 0)
    }
}

pub fn load_file(path: &std::path::Path) -> Result<Song> {
    let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&data).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse(data: &[u8]) -> Result<Song> {
    let mut r = Reader { data, pos: 0, version: (0, 0, 0) };
    let version = r.byte_size_string(30)?;
    r.version = parse_version(&version).with_context(|| format!("unsupported file version '{version}'"))?;
    let mut song = Song::default();
    let clipboard = version.starts_with("CLIPBOARD");
    if clipboard {
        bail!("Guitar Pro clipboard files are not supported");
    }

    // Song information
    song.title = r.int_byte_size_string()?;
    let _subtitle = r.int_byte_size_string()?;
    song.artist = r.int_byte_size_string()?;
    song.album = r.int_byte_size_string()?;
    let _words = r.int_byte_size_string()?;
    if r.gp5() {
        let _music = r.int_byte_size_string()?;
    }
    let _copyright = r.int_byte_size_string()?;
    let _tab = r.int_byte_size_string()?;
    let _instructions = r.int_byte_size_string()?;
    let notice_count = r.i32()?;
    for _ in 0..notice_count.max(0) {
        r.int_byte_size_string()?;
    }

    let mut triplet_feel = TripletFeel::None;
    if !r.gp5() && r.bool()? {
        triplet_feel = TripletFeel::Eighth;
    }
    if !r.gp3() {
        // Lyrics: track choice + 5 lines (not used by the player).
        r.i32()?;
        for _ in 0..5 {
            r.i32()?;
            r.int_size_string()?;
        }
    }
    if r.gp5() {
        // RSE master effect
        if r.version > (5, 0, 0) {
            r.i32()?;
            r.i32()?;
            r.skip(11)?;
        }
        // Page setup
        r.skip(4 * 2 + 4 * 4 + 4 + 2)?;
        for _ in 0..10 {
            r.int_byte_size_string()?;
        }
        let _tempo_name = r.int_byte_size_string()?;
    }
    song.tempo = r.i32()?.max(1) as u32;
    if r.gp5() {
        if r.version > (5, 0, 0) {
            r.bool()?; // hide tempo
        }
        r.i8()?; // key
        r.i32()?; // octave
    } else {
        r.i32()?; // key
        if r.gp4() {
            r.i8()?; // octave
        }
    }

    let channels = read_midi_channels(&mut r)?;
    if r.gp5() {
        // Directions (coda/segno etc.) - 19 shorts
        r.skip(19 * 2)?;
        r.i32()?; // master reverb
    }
    let measure_count = r.i32()?.max(0) as usize;
    let track_count = r.i32()?.max(0) as usize;

    // Measure headers
    let mut start = TPQ;
    for i in 0..measure_count {
        let prev = song.measure_headers.last().cloned();
        let mut h = read_measure_header(&mut r, i + 1, prev.as_ref(), &song.measure_headers)
            .with_context(|| format!("measure header {}", i + 1))?;
        if !r.gp5() {
            h.triplet_feel = triplet_feel;
        }
        h.start = start;
        start += h.length();
        song.measure_headers.push(h);
    }

    // Tracks
    for i in 0..track_count {
        let t = read_track(&mut r, i, &channels).with_context(|| format!("track {}", i + 1))?;
        song.tracks.push(t);
    }
    if r.gp5() {
        r.skip(if r.gp500() { 2 } else { 1 })?;
    }

    // Measures
    let trace = std::env::var_os("GP_TRACE").is_some();
    for mi in 0..measure_count {
        let hstart = song.measure_headers[mi].start;
        for ti in 0..track_count {
            if trace {
                eprintln!("M {} {} {}", mi + 1, ti + 1, r.pos);
            }
            let string_count = song.tracks[ti].strings.len();
            let m = read_measure(&mut r, hstart, string_count)
                .with_context(|| format!("track {}, measure {}", ti + 1, mi + 1))?;
            song.tracks[ti].measures.push(m);
        }
    }

    resolve_ties(&mut song);
    Ok(song)
}

fn parse_version(v: &str) -> Option<(u8, u8, u8)> {
    let known = [
        ("FICHIER GUITAR PRO v3.00", (3, 0, 0)),
        ("FICHIER GUITAR PRO v4.00", (4, 0, 0)),
        ("FICHIER GUITAR PRO v4.06", (4, 0, 6)),
        ("FICHIER GUITAR PRO L4.06", (4, 0, 6)),
        ("FICHIER GUITAR PRO v5.00", (5, 0, 0)),
        ("FICHIER GUITAR PRO v5.10", (5, 1, 0)),
    ];
    known.iter().find(|(k, _)| *k == v).map(|(_, t)| *t)
}

fn read_midi_channels(r: &mut Reader) -> Result<Vec<MidiChannel>> {
    let mut channels = Vec::with_capacity(64);
    for i in 0..64 {
        let mut instrument = r.i32()?;
        if i % 16 == 9 && instrument == -1 {
            instrument = 0;
        }
        let to_short = |d: i8| -> u8 { (((d as i32) << 3) - 1).clamp(-1, 32767).wrapping_add(1).min(127) as u8 };
        let volume = to_short(r.i8()?);
        let balance = to_short(r.i8()?);
        r.skip(4)?; // chorus, reverb, phaser, tremolo
        r.skip(2)?;
        channels.push(MidiChannel { instrument, volume, balance });
    }
    Ok(channels)
}

fn read_measure_header(
    r: &mut Reader,
    number: usize,
    prev: Option<&MeasureHeader>,
    previous_headers: &[MeasureHeader],
) -> Result<MeasureHeader> {
    if r.gp5() && prev.is_some() {
        r.skip(1)?;
    }
    let flags = r.u8()?;
    let mut h = MeasureHeader {
        number,
        numerator: prev.map(|p| p.numerator).unwrap_or(4),
        denominator: prev.map(|p| p.denominator).unwrap_or(4),
        repeat_close: -1,
        ..Default::default()
    };
    if flags & 0x01 != 0 {
        h.numerator = r.i8()?.max(1) as u8;
    }
    if flags & 0x02 != 0 {
        h.denominator = r.i8()?.max(1) as u8;
    }
    h.repeat_open = flags & 0x04 != 0;
    if flags & 0x08 != 0 {
        h.repeat_close = r.i8()? as i32;
    }
    if r.gp5() {
        if flags & 0x20 != 0 {
            h.marker = Some(r.int_byte_size_string()?);
            r.skip(4)?; // color
        }
        if flags & 0x40 != 0 {
            r.skip(2)?; // key signature
        }
        if flags & 0x10 != 0 {
            h.repeat_alternative = r.u8()?;
        }
        h.double_bar = flags & 0x80 != 0;
        if h.repeat_close > -1 {
            h.repeat_close -= 1;
        }
        if flags & 0x03 != 0 {
            r.skip(4)?; // beams
        }
        if flags & 0x10 == 0 {
            r.skip(1)?;
        }
        h.triplet_feel = match r.u8()? {
            1 => TripletFeel::Eighth,
            2 => TripletFeel::Sixteenth,
            _ => TripletFeel::None,
        };
    } else {
        if flags & 0x10 != 0 {
            let value = r.u8()?;
            let mut existing = 0u8;
            for ph in previous_headers.iter().rev() {
                if ph.repeat_open {
                    break;
                }
                existing |= ph.repeat_alternative;
            }
            let mask = if value >= 8 { 0xFF } else { (1u8 << value) - 1 };
            h.repeat_alternative = mask ^ existing;
        }
        if flags & 0x20 != 0 {
            h.marker = Some(r.int_byte_size_string()?);
            r.skip(4)?;
        }
        if flags & 0x40 != 0 {
            r.skip(2)?;
        }
        h.double_bar = flags & 0x80 != 0;
    }
    Ok(h)
}

fn read_track(r: &mut Reader, index: usize, channels: &[MidiChannel]) -> Result<Track> {
    if r.gp5() && (index == 0 || r.gp500()) {
        r.skip(1)?;
    }
    let flags = r.u8()?;
    let mut t = Track { percussion: flags & 0x01 != 0, ..Default::default() };
    t.name = r.byte_size_string(40)?;
    let string_count = r.i32()?.clamp(0, 7) as usize;
    for i in 0..7 {
        let tuning = r.i32()?;
        if i < string_count {
            t.strings.push(tuning);
        }
    }
    r.i32()?; // port
    let channel_index = r.i32()? - 1;
    r.i32()?; // effect channel
    if channel_index >= 0 && (channel_index as usize) < channels.len() {
        let c = channels[channel_index as usize];
        t.program = c.instrument.max(0);
        t.volume = c.volume;
        t.balance = c.balance;
        if channel_index % 16 == 9 {
            t.percussion = true;
        }
    } else {
        t.volume = 100;
        t.balance = 64;
    }
    r.i32()?; // fret count
    t.capo = r.i32()?;
    let red = r.u8()?;
    let green = r.u8()?;
    let blue = r.u8()?;
    r.skip(1)?;
    t.color = (red, green, blue);

    if r.gp5() {
        r.i16()?; // flags2
        r.u8()?; // auto accentuation
        r.u8()?; // bank
        r.u8()?; // humanize
        r.skip(4 * 3)?; // clef transpose x2, unknown
        r.skip(12)?;
        // RSE instrument
        r.i32()?;
        r.i32()?;
        r.i32()?;
        if r.gp500() {
            r.i16()?;
            r.skip(1)?;
        } else {
            r.i32()?;
        }
        if r.version > (5, 0, 0) {
            r.skip(4)?; // equalizer
            r.int_byte_size_string()?;
            r.int_byte_size_string()?;
        }
    }
    Ok(t)
}

fn read_measure(r: &mut Reader, start: u32, string_count: usize) -> Result<Measure> {
    let voice_count = if r.gp5() { 2 } else { 1 };
    let mut m = Measure::default();
    for _ in 0..voice_count {
        let mut voice = Voice::default();
        let beat_count = r.i32()?;
        if !(0..=1024).contains(&beat_count) {
            bail!("implausible beat count {beat_count}");
        }
        let mut t = start;
        for _ in 0..beat_count {
            let beat = read_beat(r, t, string_count)?;
            t += beat.ticks();
            voice.beats.push(beat);
        }
        m.voices.push(voice);
    }
    if r.gp5() && r.pos < r.data.len() {
        r.u8()?; // line break (may be missing at end of file)
    }
    Ok(m)
}

fn read_duration(r: &mut Reader, flags: u8) -> Result<Duration> {
    let v = r.i8()?;
    let value = 1u32 << ((v as i32 + 2).clamp(0, 7));
    let mut d = Duration { value: value as u8, dotted: flags & 0x01 != 0, ..Default::default() };
    if flags & 0x20 != 0 {
        let t = r.i32()?;
        d.tuplet = match t {
            3 => (3, 2),
            5 => (5, 4),
            6 => (6, 4),
            7 => (7, 4),
            9 => (9, 8),
            10 => (10, 8),
            11 => (11, 8),
            12 => (12, 8),
            13 => (13, 8),
            _ => (1, 1),
        };
    }
    Ok(d)
}

fn read_beat(r: &mut Reader, start: u32, string_count: usize) -> Result<Beat> {
    let flags = r.u8()?;
    let mut beat = Beat { start, ..Default::default() };
    beat.status = if flags & 0x40 != 0 {
        match r.u8()? {
            0 => BeatStatus::Empty,
            2 => BeatStatus::Rest,
            _ => BeatStatus::Normal,
        }
    } else {
        BeatStatus::Normal
    };
    beat.duration = read_duration(r, flags)?;
    let mut note_fx = NoteEffects::default();
    if flags & 0x02 != 0 {
        read_chord(r, string_count)?;
    }
    if flags & 0x04 != 0 {
        let text = r.int_byte_size_string()?;
        if !text.trim().is_empty() {
            beat.text = Some(text);
        }
    }
    if flags & 0x08 != 0 {
        beat.effects = read_beat_effects(r, &mut note_fx)?;
    }
    if flags & 0x10 != 0 {
        beat.tempo_change = read_mix_table(r)?;
    }
    // Notes
    let string_flags = r.u8()?;
    for s in 1..=string_count.min(7) {
        if string_flags & (1 << (7 - s)) != 0 {
            let note = read_note(r, s as u8, &note_fx)?;
            beat.notes.push(note);
        }
    }
    if r.gp5() {
        let flags2 = r.i16()?;
        if flags2 & 0x0800 != 0 {
            r.u8()?;
        }
    }
    Ok(beat)
}

fn read_chord(r: &mut Reader, _string_count: usize) -> Result<()> {
    let new_format = r.bool()?;
    if !new_format {
        r.int_byte_size_string()?;
        let first_fret = r.i32()?;
        if first_fret != 0 {
            r.skip(6 * 4)?;
        }
        return Ok(());
    }
    if r.gp3() {
        r.bool()?;
        r.skip(3)?;
        r.skip(4 * 5)?; // root, type, extension, bass, tonality
        r.bool()?;
        r.byte_size_string(22)?;
        r.skip(4 * 3)?; // fifth, ninth, eleventh
        r.i32()?; // first fret
        r.skip(6 * 4)?;
        r.skip(4 + 2 * 4 * 3)?; // barres
        r.skip(7)?;
        r.skip(1)?;
    } else {
        r.bool()?;
        r.skip(3)?;
        r.skip(3)?; // root, type, extension (u8)
        r.skip(4 * 2)?; // bass, tonality
        r.bool()?;
        r.byte_size_string(22)?;
        r.skip(3)?; // fifth, ninth, eleventh
        r.i32()?;
        r.skip(7 * 4)?;
        r.skip(1 + 5 * 3)?; // barres
        r.skip(7)?;
        r.skip(1)?;
        r.skip(7)?; // fingerings
        r.bool()?;
    }
    Ok(())
}

fn read_bend(r: &mut Reader) -> Result<Option<Bend>> {
    r.i8()?; // type
    r.i32()?; // value
    let count = r.i32()?;
    if !(0..=100).contains(&count) {
        bail!("implausible bend point count {count}");
    }
    let mut bend = Bend::default();
    for _ in 0..count {
        let pos = r.i32()? as f32 / BEND_POSITION;
        let value = r.i32()? as f32 / BEND_SEMITONE;
        r.bool()?; // vibrato
        bend.points.push((pos.clamp(0.0, 1.0), value));
    }
    Ok(if bend.points.is_empty() { None } else { Some(bend) })
}

fn read_stroke(r: &mut Reader) -> Result<Option<(StrokeDir, u8)>> {
    let down = r.i8()?;
    let up = r.i8()?;
    let to_value = |v: i8| -> u8 {
        match v {
            1 => 128,
            2 => 64,
            3 => 32,
            4 => 16,
            5 => 8,
            6 => 4,
            _ => 64,
        }
    };
    let mut s = if up > 0 {
        Some((StrokeDir::Up, to_value(up)))
    } else if down > 0 {
        Some((StrokeDir::Down, to_value(down)))
    } else {
        None
    };
    if r.gp5() {
        // GP5 stores the directions swapped.
        if let Some((d, v)) = s {
            s = Some((if d == StrokeDir::Up { StrokeDir::Down } else { StrokeDir::Up }, v));
        }
    }
    Ok(s)
}

fn read_beat_effects(r: &mut Reader, note_fx: &mut NoteEffects) -> Result<BeatEffects> {
    let mut fx = BeatEffects::default();
    if r.gp3() {
        let flags1 = r.u8()?;
        note_fx.vibrato |= flags1 & 0x01 != 0;
        fx.wide_vibrato = flags1 & 0x02 != 0;
        fx.fade_in = flags1 & 0x10 != 0;
        if flags1 & 0x20 != 0 {
            let slap = r.u8()?;
            fx.slap = slap;
            let value = r.i32()?;
            if slap == 0 {
                let v = -(value as f32) / BEND_SEMITONE;
                fx.tremolo_bar = Some(Bend { points: vec![(0.0, 0.0), (0.5, v), (1.0, 0.0)] });
            }
        }
        if flags1 & 0x40 != 0 {
            fx.stroke = read_stroke(r)?;
        }
        if flags1 & 0x04 != 0 {
            note_fx.harmonic = Some(Harmonic::Natural);
        }
        if flags1 & 0x08 != 0 {
            note_fx.harmonic = Some(Harmonic::Artificial(12));
        }
    } else {
        let flags1 = r.u8()?;
        let flags2 = r.u8()?;
        fx.wide_vibrato = flags1 & 0x02 != 0;
        fx.fade_in = flags1 & 0x10 != 0;
        if flags1 & 0x20 != 0 {
            fx.slap = r.u8()?;
        }
        if flags2 & 0x04 != 0 {
            fx.tremolo_bar = read_bend(r)?;
        }
        if flags1 & 0x40 != 0 {
            fx.stroke = read_stroke(r)?;
        }
        if flags2 & 0x02 != 0 {
            r.i8()?; // pick stroke
        }
    }
    Ok(fx)
}

/// Reads a mix table change and returns a tempo change, if any.
fn read_mix_table(r: &mut Reader) -> Result<Option<u32>> {
    r.i8()?; // instrument
    if r.gp5() {
        r.skip(4 * 3)?;
        if r.gp500() {
            r.i16()?;
            r.skip(1)?;
            r.skip(1)?;
        } else {
            r.i32()?;
        }
    }
    let mut values = [0i8; 6];
    for v in values.iter_mut() {
        *v = r.i8()?;
    }
    if r.gp5() {
        r.int_byte_size_string()?;
    }
    let tempo = r.i32()?;
    for v in values.iter() {
        if *v >= 0 {
            r.i8()?;
        }
    }
    if tempo >= 0 {
        r.i8()?;
        if r.version > (5, 0, 0) {
            r.bool()?;
        }
    }
    if !r.gp3() {
        r.i8()?; // apply-to-all-tracks flags
    }
    if r.gp5() {
        r.i8()?; // wah
        if r.version > (5, 0, 0) {
            r.int_byte_size_string()?;
            r.int_byte_size_string()?;
        }
    }
    Ok(if tempo > 0 { Some(tempo as u32) } else { None })
}

fn unpack_velocity(dyn_: i8) -> u8 {
    (15 + 16 * dyn_ as i32 - 16).clamp(1, 127) as u8
}

fn read_note(r: &mut Reader, string: u8, beat_fx: &NoteEffects) -> Result<Note> {
    let flags = r.u8()?;
    let mut n = Note { string, velocity: 95, effects: beat_fx.clone(), ..Default::default() };
    n.heavy_accent = r.gp5() && flags & 0x02 != 0;
    n.ghost = flags & 0x04 != 0;
    n.accent = flags & 0x40 != 0;
    if flags & 0x20 != 0 {
        n.kind = match r.u8()? {
            2 => NoteKind::Tie,
            3 => NoteKind::Dead,
            _ => NoteKind::Normal,
        };
    }
    if !r.gp5() && flags & 0x01 != 0 {
        r.i8()?;
        r.i8()?;
    }
    if flags & 0x10 != 0 {
        n.velocity = unpack_velocity(r.i8()?);
    }
    if flags & 0x20 != 0 {
        let fret = r.i8()? as i32;
        n.fret = if (0..100).contains(&fret) { fret } else { 0 };
    }
    if flags & 0x80 != 0 {
        r.i8()?;
        r.i8()?;
    }
    if r.gp5() {
        if flags & 0x01 != 0 {
            r.f64()?;
        }
        r.u8()?; // flags2
    }
    if flags & 0x08 != 0 {
        read_note_effects(r, &mut n)?;
    }
    Ok(n)
}

fn read_note_effects(r: &mut Reader, n: &mut Note) -> Result<()> {
    let fx = &mut n.effects;
    if r.gp3() {
        let flags = r.u8()?;
        fx.hammer = flags & 0x02 != 0;
        fx.let_ring = flags & 0x08 != 0;
        if flags & 0x01 != 0 {
            fx.bend = read_bend(r)?;
        }
        if flags & 0x10 != 0 {
            fx.grace = Some(read_grace(r)?);
        }
        if flags & 0x04 != 0 {
            fx.slides = vec![Slide::ShiftTo];
        }
        return Ok(());
    }
    let flags1 = r.u8()?;
    let flags2 = r.u8()?;
    fx.hammer = flags1 & 0x02 != 0;
    fx.let_ring = flags1 & 0x08 != 0;
    fx.staccato = flags2 & 0x01 != 0;
    fx.palm_mute = flags2 & 0x02 != 0;
    fx.vibrato |= flags2 & 0x40 != 0;
    if flags1 & 0x01 != 0 {
        fx.bend = read_bend(r)?;
    }
    if flags1 & 0x10 != 0 {
        fx.grace = Some(read_grace(r)?);
    }
    if flags2 & 0x04 != 0 {
        let v = r.i8()?;
        let value: u32 = match v {
            1 => 8,
            2 => 16,
            _ => 32,
        };
        fx.tremolo_picking = Some(TPQ * 4 / value);
    }
    if flags2 & 0x08 != 0 {
        if r.gp5() {
            let t = r.u8()?;
            let mut slides = Vec::new();
            if t & 0x01 != 0 {
                slides.push(Slide::ShiftTo);
            }
            if t & 0x02 != 0 {
                slides.push(Slide::LegatoTo);
            }
            if t & 0x04 != 0 {
                slides.push(Slide::OutDown);
            }
            if t & 0x08 != 0 {
                slides.push(Slide::OutUp);
            }
            if t & 0x10 != 0 {
                slides.push(Slide::InFromBelow);
            }
            if t & 0x20 != 0 {
                slides.push(Slide::InFromAbove);
            }
            fx.slides = slides;
        } else {
            fx.slides = match r.i8()? {
                -2 => vec![Slide::InFromAbove],
                -1 => vec![Slide::InFromBelow],
                1 => vec![Slide::ShiftTo],
                2 => vec![Slide::LegatoTo],
                3 => vec![Slide::OutDown],
                4 => vec![Slide::OutUp],
                _ => vec![],
            };
        }
    }
    if flags2 & 0x10 != 0 {
        let t = r.i8()?;
        fx.harmonic = if r.gp5() {
            match t {
                1 => Some(Harmonic::Natural),
                2 => {
                    r.u8()?;
                    r.i8()?;
                    let octave = r.u8()?;
                    Some(Harmonic::Artificial(12 * (octave as i32).max(1)))
                }
                3 => {
                    let fret = r.u8()? as i32;
                    Some(Harmonic::Tapped(fret))
                }
                4 => Some(Harmonic::Pinch),
                5 => Some(Harmonic::Semi),
                _ => None,
            }
        } else {
            match t {
                1 => Some(Harmonic::Natural),
                3 => Some(Harmonic::Tapped(n.fret + 12)),
                4 => Some(Harmonic::Pinch),
                5 => Some(Harmonic::Semi),
                15 => Some(Harmonic::Artificial(19)),
                17 => Some(Harmonic::Artificial(24)),
                22 => Some(Harmonic::Artificial(12)),
                _ => None,
            }
        };
    }
    if flags2 & 0x20 != 0 {
        let fret = r.i8()? as i32;
        let period: u32 = match r.i8()? {
            1 => 16,
            2 => 32,
            _ => 64,
        };
        fx.trill = Some((fret, TPQ * 4 / period));
    }
    Ok(())
}

fn read_grace(r: &mut Reader) -> Result<Grace> {
    if r.gp5() {
        let fret = r.u8()? as i32;
        let velocity = unpack_velocity(r.u8()? as i8);
        let transition = r.u8()?;
        let duration = 1u8 << (7 - r.u8()?.min(7));
        let flags = r.u8()?;
        Ok(Grace {
            fret,
            velocity,
            duration,
            transition: grace_transition(transition as i8),
            dead: flags & 0x01 != 0,
            on_beat: flags & 0x02 != 0,
        })
    } else {
        let fret = r.i8()? as i32;
        let velocity = unpack_velocity(r.u8()? as i8);
        let duration = 1u8 << (7 - r.u8()?.min(7));
        let transition = r.i8()?;
        Ok(Grace {
            fret: fret.max(0),
            velocity,
            duration,
            transition: grace_transition(transition),
            dead: fret == -1,
            on_beat: false,
        })
    }
}

fn grace_transition(v: i8) -> GraceTransition {
    match v {
        1 => GraceTransition::Slide,
        2 => GraceTransition::Bend,
        3 => GraceTransition::Hammer,
        _ => GraceTransition::None,
    }
}

/// Tied notes in GP files don't always carry the fret; copy it from the previous note on the same string.
fn resolve_ties(song: &mut Song) {
    for track in song.tracks.iter_mut() {
        let voices = track.measures.iter().map(|m| m.voices.len()).max().unwrap_or(0);
        for v in 0..voices {
            let mut last = [None::<i32>; 8];
            for m in track.measures.iter_mut() {
                let Some(voice) = m.voices.get_mut(v) else { continue };
                for beat in voice.beats.iter_mut() {
                    if beat.status == BeatStatus::Empty {
                        continue;
                    }
                    for note in beat.notes.iter_mut() {
                        let s = (note.string as usize).min(7);
                        if note.kind == NoteKind::Tie {
                            if let Some(f) = last[s] {
                                note.fret = f;
                            }
                        }
                        last[s] = Some(note.fret);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_ticks() {
        let d = Duration { value: 8, dotted: true, ..Default::default() };
        assert_eq!(d.ticks(), 720);
        let t = Duration { value: 8, tuplet: (3, 2), ..Default::default() };
        assert_eq!(t.ticks(), 320);
    }
}
