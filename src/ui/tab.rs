//! Tablature layout and rendering.

use eframe::egui;
use egui::epaint::{CubicBezierShape, QuadraticBezierShape};
use egui::{Align2, Color32, FontId, Pos2, Rect, Shape, Stroke, Ui, pos2, vec2};

use crate::model::*;

pub const LEFT: f32 = 46.0;
pub const RIGHT: f32 = 16.0;
const TOP: f32 = 64.0;
const SP: f32 = 13.0;
const BOTTOM: f32 = 42.0;
const GAP: f32 = 16.0;

const LANE_TEXT: f32 = 10.0;
const LANE_ORN: f32 = 27.0;
const LANE_PM: f32 = 42.0;

pub const BG: Color32 = Color32::from_rgb(18, 20, 24);
const INK: Color32 = Color32::from_rgb(222, 225, 230);
const LINE: Color32 = Color32::from_rgb(78, 84, 94);
const SOFT: Color32 = Color32::from_rgb(140, 146, 156);
const ACCENT: Color32 = Color32::from_rgb(255, 120, 70);
const MARKER: Color32 = Color32::from_rgb(110, 170, 255);

const F_PM: u8 = 1;
const F_LR: u8 = 2;
const F_VIB: u8 = 4;
const F_WBAR: u8 = 8;
const F_TRILL: u8 = 16;
const F_TREM: u8 = 32;

#[derive(Clone, Copy)]
struct NoteRef {
    voice: usize,
    beat: usize,
    note: usize,
}

struct Col {
    tick: u32,
    x: f32,
    flags: u8,
    notes: Vec<NoteRef>,
    harm: Option<&'static str>,
    tempo: Option<u32>,
    text: Option<usize>,
}

pub struct MLay {
    pub row: usize,
    pub x: f32,
    pub w: f32,
    pad_left: f32,
    pad_right: f32,
    len: u32,
    cols: Vec<Col>,
    show_sig: bool,
}

pub struct Row {
    pub y: f32,
    pub h: f32,
    pub first: usize,
    pub last: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum LinkKind {
    Hammer,
    Pull,
    SlideUp,
    SlideDown,
}

struct Link {
    from_tick: u32,
    to_m: usize,
    to_tick: u32,
    string: usize,
    kind: LinkKind,
}

pub struct TabLayout {
    pub track: usize,
    pub width: f32,
    pub rows: Vec<Row>,
    pub meas: Vec<MLay>,
    pub height: f32,
    lines: usize,
    drums: bool,
    links: Vec<Vec<Link>>,
}

fn col_width(gap: u32) -> f32 {
    let g = gap.max(1) as f32 / 240.0;
    (25.0 + 11.0 * g.log2()).clamp(17.0, 72.0)
}

fn drum_lane(key: i32) -> usize {
    match key {
        49 | 52 | 55 | 57 => 0,
        51 | 53 | 59 => 1,
        42 | 44 | 46 => 2,
        48 | 50 => 3,
        37..=40 => 4,
        41 | 43 | 45 | 47 => 5,
        35 | 36 => 6,
        _ => 3,
    }
}

const DRUM_LABELS: [&str; 7] = ["CR", "RD", "HH", "T1", "SN", "T2", "BD"];

impl TabLayout {
    pub fn build(song: &Song, track_idx: usize, width: f32) -> TabLayout {
        let track = &song.tracks[track_idx];
        let drums = track.percussion;
        let lines = if drums { 7 } else { track.strings.len().max(1) };
        let mut meas = Vec::with_capacity(song.measure_headers.len());
        let mut prev_sig = (0u8, 0u8);
        for (mi, h) in song.measure_headers.iter().enumerate() {
            let len = h.length();
            let mut cols: Vec<Col> = Vec::new();
            if let Some(m) = track.measures.get(mi) {
                let mut ticks: Vec<u32> = m
                    .voices
                    .iter()
                    .flat_map(|v| v.beats.iter())
                    .filter(|b| b.status != BeatStatus::Empty)
                    .map(|b| b.start.saturating_sub(h.start).min(len.saturating_sub(1)))
                    .collect();
                ticks.sort_unstable();
                ticks.dedup();
                cols = ticks
                    .into_iter()
                    .map(|t| Col { tick: t, x: 0.0, flags: 0, notes: vec![], harm: None, tempo: None, text: None })
                    .collect();
                for (vi, v) in m.voices.iter().enumerate() {
                    for (bi, b) in v.beats.iter().enumerate() {
                        if b.status == BeatStatus::Empty {
                            continue;
                        }
                        let rel = b.start.saturating_sub(h.start).min(len.saturating_sub(1));
                        let Ok(ci) = cols.binary_search_by_key(&rel, |c| c.tick) else {
                            continue;
                        };
                        let c = &mut cols[ci];
                        if b.effects.wide_vibrato {
                            c.flags |= F_VIB;
                        }
                        if b.effects.tremolo_bar.is_some() {
                            c.flags |= F_WBAR;
                        }
                        if b.tempo_change.is_some() {
                            c.tempo = b.tempo_change;
                        }
                        if b.text.is_some() && vi == 0 {
                            c.text = Some(bi);
                        }
                        for (ni, n) in b.notes.iter().enumerate() {
                            let e = &n.effects;
                            if e.palm_mute {
                                c.flags |= F_PM;
                            }
                            if e.let_ring {
                                c.flags |= F_LR;
                            }
                            if e.vibrato {
                                c.flags |= F_VIB;
                            }
                            if e.trill.is_some() {
                                c.flags |= F_TRILL;
                            }
                            if e.tremolo_picking.is_some() {
                                c.flags |= F_TREM;
                            }
                            if let Some(hm) = &e.harmonic {
                                c.harm = Some(hm.short_label());
                            }
                            c.notes.push(NoteRef { voice: vi, beat: bi, note: ni });
                        }
                    }
                }
            }
            if cols.is_empty() {
                cols.push(Col { tick: 0, x: 0.0, flags: 0, notes: vec![], harm: None, tempo: None, text: None });
            }
            let sig = (h.numerator, h.denominator);
            let show_sig = sig != prev_sig;
            prev_sig = sig;
            let pad_left = 10.0 + if show_sig { 18.0 } else { 0.0 } + if h.repeat_open { 10.0 } else { 0.0 };
            let pad_right = 6.0 + if h.repeat_close > 0 { 10.0 } else { 0.0 };
            let mut cum = 0.0;
            for i in 0..cols.len() {
                let next = cols.get(i + 1).map(|c| c.tick).unwrap_or(len);
                cols[i].x = cum;
                cum += col_width(next.saturating_sub(cols[i].tick));
            }
            meas.push(MLay { row: 0, x: 0.0, w: pad_left + cum + pad_right, pad_left, pad_right, len, cols, show_sig });
        }

        // Break into rows, then stretch each row to the full width.
        let avail = (width - LEFT - RIGHT).max(200.0);
        let row_h = TOP + (lines as f32 - 1.0) * SP + BOTTOM + GAP;
        let mut rows: Vec<Row> = Vec::new();
        let mut start = 0;
        while start < meas.len() {
            let mut end = start;
            let mut wsum = meas[start].w;
            while end + 1 < meas.len() && wsum + meas[end + 1].w <= avail {
                end += 1;
                wsum += meas[end].w;
            }
            let last_row = end + 1 >= meas.len();
            let pads: f32 = meas[start..=end].iter().map(|m| m.pad_left + m.pad_right).sum();
            let content = wsum - pads;
            let f = if last_row && wsum < avail * 0.7 { 1.0 } else { ((avail - pads) / content.max(1.0)).max(0.3) };
            let row = rows.len();
            let mut x = LEFT;
            for m in &mut meas[start..=end] {
                let content_w = m.w - m.pad_left - m.pad_right;
                for c in &mut m.cols {
                    c.x = m.pad_left + c.x * f + 8.0;
                }
                m.w = m.pad_left + content_w * f + m.pad_right;
                m.x = x;
                m.row = row;
                x += m.w;
            }
            rows.push(Row { y: row as f32 * row_h + 8.0, h: row_h, first: start, last: end });
            start = end + 1;
        }
        let height = rows.last().map(|r| r.y + r.h).unwrap_or(0.0) + 40.0;

        let links = if drums { (0..meas.len()).map(|_| vec![]).collect() } else { build_links(song, track) };
        TabLayout { track: track_idx, width, rows, meas, height, lines, drums, links }
    }

    fn staff_top(&self, row: usize) -> f32 {
        self.rows[row].y + TOP
    }

    fn line_y(&self, row: usize, line: usize) -> f32 {
        self.staff_top(row) + line as f32 * SP
    }

    fn staff_bottom(&self, row: usize) -> f32 {
        self.line_y(row, self.lines - 1)
    }

    /// x (content coordinates) of a tick within a measure.
    pub fn x_at(&self, mi: usize, rel: f64) -> f32 {
        let m = &self.meas[mi];
        let k = m.cols.partition_point(|c| (c.tick as f64) <= rel);
        if k == 0 {
            return m.x + m.cols[0].x;
        }
        let (t0, x0) = (m.cols[k - 1].tick as f64, m.cols[k - 1].x);
        let (t1, x1) = match m.cols.get(k) {
            Some(c) => (c.tick as f64, c.x),
            None => (m.len as f64, m.w - m.pad_right + 2.0),
        };
        let f = ((rel - t0) / (t1 - t0).max(1.0)).clamp(0.0, 1.0) as f32;
        m.x + x0 + (x1 - x0) * f
    }

    /// Rect (content coordinates) of a row's staff area, used for scrolling.
    pub fn row_rect(&self, row: usize) -> Rect {
        let r = &self.rows[row];
        Rect::from_min_size(pos2(0.0, r.y), vec2(self.width, r.h))
    }

    /// Hit test: measure index and the nearest beat tick (relative to measure start).
    pub fn hit(&self, p: Pos2) -> Option<(usize, u32)> {
        let row = self.rows.iter().position(|r| p.y >= r.y && p.y < r.y + r.h)?;
        let r = &self.rows[row];
        let mi = (r.first..=r.last).find(|&i| p.x < self.meas[i].x + self.meas[i].w).unwrap_or(r.last);
        let m = &self.meas[mi];
        let lx = p.x - m.x;
        let ci = m
            .cols
            .iter()
            .enumerate()
            .min_by(|a, b| (a.1.x - lx).abs().total_cmp(&(b.1.x - lx).abs()))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let tick = if lx < m.cols[0].x - 6.0 { 0 } else { m.cols[ci].tick };
        Some((mi, tick))
    }
}

fn build_links(song: &Song, track: &Track) -> Vec<Vec<Link>> {
    let n_meas = song.measure_headers.len();
    let mut links: Vec<Vec<Link>> = (0..n_meas).map(|_| vec![]).collect();
    for vi in 0..2 {
        // per string: (measure, tick, fret, hammer, slide)
        let mut last: Vec<Option<(usize, u32, i32, bool, bool)>> = vec![None; track.strings.len().max(1)];
        for (mi, h) in song.measure_headers.iter().enumerate() {
            let Some(v) = track.measures.get(mi).and_then(|m| m.voices.get(vi)) else {
                continue;
            };
            for b in &v.beats {
                if b.status == BeatStatus::Empty {
                    continue;
                }
                if b.status == BeatStatus::Rest {
                    last.iter_mut().for_each(|l| *l = None);
                    continue;
                }
                let rel = b.start.saturating_sub(h.start);
                for n in &b.notes {
                    let s = (n.string as usize).saturating_sub(1);
                    if s >= last.len() {
                        continue;
                    }
                    if let Some((pm, pt, pf, hammer, slide)) = last[s] {
                        if n.kind != NoteKind::Tie && (hammer || slide) {
                            let kind = if slide {
                                if n.fret >= pf { LinkKind::SlideUp } else { LinkKind::SlideDown }
                            } else if n.fret > pf {
                                LinkKind::Hammer
                            } else {
                                LinkKind::Pull
                            };
                            links[pm].push(Link { from_tick: pt, to_m: mi, to_tick: rel, string: s, kind });
                        }
                    }
                    let e = &n.effects;
                    let slide = e.has_slide(Slide::LegatoTo) || e.has_slide(Slide::ShiftTo);
                    last[s] = Some((mi, rel, n.fret, e.hammer, slide));
                }
            }
        }
    }
    links
}

fn bend_label(semis: f32) -> String {
    let steps = (semis.abs() / 2.0 * 4.0).round() as i32; // quarter steps
    let whole = steps / 4;
    let frac = match steps % 4 {
        1 => "¼",
        2 => "½",
        3 => "¾",
        _ => "",
    };
    match (whole, frac) {
        (0, "") => String::new(),
        (1, "") => "full".into(),
        (0, f) => f.into(),
        (w, f) => format!("{w}{f}"),
    }
}

pub struct Paint<'a> {
    pub song: &'a Song,
    pub layout: &'a TabLayout,
    pub origin: Pos2,
    pub viewport: Rect,
    /// (measure index, tick within measure) of the playback cursor.
    pub cursor: Option<(usize, f64)>,
    pub selection: Option<(usize, usize)>,
    pub loop_active: bool,
}

impl Paint<'_> {
    fn p(&self, x: f32, y: f32) -> Pos2 {
        self.origin + vec2(x, y)
    }

    pub fn draw(&self, ui: &Ui) {
        let painter = ui.painter();
        let lay = self.layout;
        for (ri, row) in lay.rows.iter().enumerate() {
            if row.y > self.viewport.max.y || row.y + row.h < self.viewport.min.y {
                continue;
            }
            self.draw_row(painter, ri);
        }
    }

    fn knockout_text(&self, painter: &egui::Painter, pos: Pos2, text: String, size: f32, color: Color32) {
        let galley = painter.layout_no_wrap(text, FontId::proportional(size), color);
        let rect = Align2::CENTER_CENTER.anchor_size(pos, galley.size());
        painter.rect_filled(rect.expand2(vec2(1.0, -1.0)), 2.0, BG);
        painter.galley(rect.min, galley, color);
    }

    fn draw_row(&self, painter: &egui::Painter, ri: usize) {
        let lay = self.layout;
        let song = self.song;
        let track = &song.tracks[lay.track];
        let row = &lay.rows[ri];
        let x0 = LEFT;
        let x1 = lay.meas[row.last].x + lay.meas[row.last].w;
        let top = lay.staff_top(ri);
        let bottom = lay.staff_bottom(ri);
        let thin = Stroke::new(1.0, LINE);

        // Selection / loop highlight.
        if let Some((a, b)) = self.selection {
            for mi in row.first..=row.last {
                if mi >= a && mi <= b {
                    let m = &lay.meas[mi];
                    let color = if self.loop_active {
                        Color32::from_rgba_unmultiplied(80, 150, 255, 45)
                    } else {
                        Color32::from_rgba_unmultiplied(160, 170, 190, 28)
                    };
                    painter.rect_filled(
                        Rect::from_min_max(self.p(m.x, top - 18.0), self.p(m.x + m.w, bottom + 30.0)),
                        0.0,
                        color,
                    );
                }
            }
        }

        // Staff lines and clef/tuning labels.
        for l in 0..lay.lines {
            let y = lay.line_y(ri, l);
            painter.line_segment([self.p(x0, y), self.p(x1, y)], thin);
            let label = if lay.drums {
                DRUM_LABELS[l].to_string()
            } else {
                note_name(track.strings.get(l).copied().unwrap_or(0))
            };
            painter.text(self.p(x0 - 10.0, y), Align2::RIGHT_CENTER, label, FontId::proportional(10.0), SOFT);
        }

        let mut pm_runs: Vec<(u8, f32, f32)> = Vec::new(); // flag, start x, end x
        for mi in row.first..=row.last {
            self.draw_measure_frame(painter, ri, mi, top, bottom);
            let m = &lay.meas[mi];
            for c in &m.cols {
                let x = m.x + c.x;
                for flag in [F_PM, F_LR] {
                    if c.flags & flag != 0 {
                        match pm_runs.iter_mut().rev().find(|r| r.0 == flag) {
                            Some(r) if r.2 >= x - 80.0 && self.is_consecutive(mi, c.tick, flag, r.2) => r.2 = x,
                            _ => pm_runs.push((flag, x, x)),
                        }
                    }
                }
            }
        }
        for (flag, a, b) in pm_runs {
            let (label, y) = if flag == F_PM { ("P.M.", LANE_PM) } else { ("let ring", LANE_PM - 13.0) };
            let y = row.y + y;
            let r = painter.text(self.p(a - 6.0, y), Align2::LEFT_CENTER, label, FontId::proportional(10.5), INK);
            let start = r.max.x - self.origin.x + 3.0;
            if b + 8.0 > start + 4.0 {
                let pts = [self.p(start, y), self.p(b + 8.0, y)];
                painter.extend(Shape::dashed_line(&pts, Stroke::new(1.0, SOFT), 3.0, 3.0));
                painter.line_segment([self.p(b + 8.0, y - 4.0), self.p(b + 8.0, y + 4.0)], Stroke::new(1.0, SOFT));
            }
        }

        for mi in row.first..=row.last {
            self.draw_links(painter, ri, mi);
        }
        for mi in row.first..=row.last {
            self.draw_notes(painter, ri, mi);
            self.draw_rhythm(painter, ri, mi, bottom);
        }

        // Playback cursor.
        if let Some((cm, rel)) = self.cursor {
            if lay.meas.get(cm).map(|m| m.row) == Some(ri) {
                let x = lay.x_at(cm, rel);
                let rect = Rect::from_min_max(self.p(x - 9.0, top - 10.0), self.p(x + 9.0, bottom + 10.0));
                painter.rect_filled(rect, 3.0, Color32::from_rgba_unmultiplied(255, 120, 40, 45));
                painter.line_segment([self.p(x, top - 10.0), self.p(x, bottom + 10.0)], Stroke::new(2.0, ACCENT));
            }
        }
    }

    /// Whether the flag run continues without a gap from the previous flagged column.
    fn is_consecutive(&self, mi: usize, tick: u32, flag: u8, prev_x: f32) -> bool {
        let lay = self.layout;
        let m = &lay.meas[mi];
        // find the column before this one (possibly in the previous measure)
        let ci = m.cols.iter().position(|c| c.tick == tick).unwrap_or(0);
        let prev = if ci > 0 {
            Some(&m.cols[ci - 1])
        } else if mi > 0 && lay.meas[mi - 1].row == m.row {
            lay.meas[mi - 1].cols.last()
        } else {
            None
        };
        match prev {
            Some(p) => p.flags & flag != 0 || p.notes.is_empty() && prev_x > 0.0,
            None => false,
        }
    }

    fn draw_measure_frame(&self, painter: &egui::Painter, ri: usize, mi: usize, top: f32, bottom: f32) {
        let lay = self.layout;
        let m = &lay.meas[mi];
        let h = &self.song.measure_headers[mi];
        let row = &lay.rows[ri];
        let bar = Stroke::new(1.0, INK);
        let thick = Stroke::new(3.0, INK);
        let right = m.x + m.w;

        // Bar lines.
        if mi == row.first {
            painter.line_segment([self.p(m.x, top), self.p(m.x, bottom)], bar);
        }
        let is_last = mi + 1 == lay.meas.len();
        if h.repeat_close > 0 {
            painter.line_segment([self.p(right - 2.0, top), self.p(right - 2.0, bottom)], thick);
            painter.line_segment([self.p(right - 6.0, top), self.p(right - 6.0, bottom)], bar);
            let mid = (top + bottom) / 2.0;
            painter.circle_filled(self.p(right - 11.0, mid - SP * 0.5 - 2.0), 2.0, INK);
            painter.circle_filled(self.p(right - 11.0, mid + SP * 0.5 + 2.0), 2.0, INK);
            if h.repeat_close > 1 {
                painter.text(
                    self.p(right - 4.0, top - 6.0),
                    Align2::RIGHT_BOTTOM,
                    format!("x{}", h.repeat_close + 1),
                    FontId::proportional(11.0),
                    INK,
                );
            }
        } else if is_last {
            painter.line_segment([self.p(right - 2.0, top), self.p(right - 2.0, bottom)], thick);
            painter.line_segment([self.p(right - 6.0, top), self.p(right - 6.0, bottom)], bar);
        } else if h.double_bar {
            painter.line_segment([self.p(right, top), self.p(right, bottom)], bar);
            painter.line_segment([self.p(right - 3.0, top), self.p(right - 3.0, bottom)], bar);
        } else {
            painter.line_segment([self.p(right, top), self.p(right, bottom)], bar);
        }
        let mut lx = m.x + 4.0;
        if h.repeat_open {
            painter.line_segment([self.p(m.x + 2.0, top), self.p(m.x + 2.0, bottom)], thick);
            painter.line_segment([self.p(m.x + 6.0, top), self.p(m.x + 6.0, bottom)], bar);
            let mid = (top + bottom) / 2.0;
            painter.circle_filled(self.p(m.x + 11.0, mid - SP * 0.5 - 2.0), 2.0, INK);
            painter.circle_filled(self.p(m.x + 11.0, mid + SP * 0.5 + 2.0), 2.0, INK);
            lx += 10.0;
        }
        if m.show_sig {
            let mid = (top + bottom) / 2.0;
            let f = FontId::proportional(17.0);
            painter.text(self.p(lx + 7.0, mid - 1.0), Align2::CENTER_BOTTOM, h.numerator.to_string(), f.clone(), INK);
            painter.text(self.p(lx + 7.0, mid + 1.0), Align2::CENTER_TOP, h.denominator.to_string(), f, INK);
        }

        // Measure number, markers, alternate endings.
        let ty = lay.rows[ri].y + LANE_TEXT;
        let r = painter.text(self.p(m.x + 1.0, top - 3.0), Align2::LEFT_BOTTOM, h.number.to_string(), FontId::proportional(9.0), ACCENT);
        let mut tx = r.max.x - self.origin.x + 6.0;
        if h.repeat_alternative != 0 {
            let endings: Vec<String> = (0..8).filter(|b| h.repeat_alternative & (1 << b) != 0).map(|b| format!("{}.", b + 1)).collect();
            let y = ty - 6.0;
            painter.line_segment([self.p(m.x + 2.0, y), self.p(right - 4.0, y)], bar);
            painter.line_segment([self.p(m.x + 2.0, y), self.p(m.x + 2.0, y + 12.0)], bar);
            painter.text(self.p(m.x + 6.0, y + 2.0), Align2::LEFT_TOP, endings.join(" "), FontId::proportional(10.0), INK);
            tx = tx.max(m.x + 30.0);
        }
        if let Some(mk) = &h.marker {
            let r = painter.text(self.p(tx, ty), Align2::LEFT_CENTER, mk, FontId::proportional(12.5), MARKER);
            tx = r.max.x - self.origin.x + 8.0;
        }
        for c in &m.cols {
            let x = m.x + c.x;
            if let Some(bpm) = c.tempo {
                let r = painter.text(
                    self.p(x.max(tx), ty),
                    Align2::LEFT_CENTER,
                    format!("♩ = {bpm}"),
                    FontId::proportional(11.5),
                    INK,
                );
                tx = r.max.x - self.origin.x + 8.0;
            }
            if let Some(bi) = c.text {
                if let Some(t) = self.song.tracks[lay.track].measures[mi].voices[0].beats[bi].text.as_deref() {
                    let r = painter.text(self.p(x.max(tx), ty), Align2::LEFT_CENTER, t, FontId::proportional(10.5), SOFT);
                    tx = r.max.x - self.origin.x + 8.0;
                }
            }
            let oy = lay.rows[ri].y + LANE_ORN;
            // Labels sit just above the vibrato lane so the two never overlap.
            let ly = oy - 12.0;
            if let Some(hl) = c.harm {
                painter.text(self.p(x, ly), Align2::CENTER_CENTER, hl, FontId::proportional(10.0), INK);
            }
            if c.flags & F_WBAR != 0 {
                painter.text(self.p(x, ly), Align2::CENTER_CENTER, "w/bar", FontId::proportional(10.0), INK);
            }
            if c.flags & F_TRILL != 0 {
                painter.text(self.p(x, oy), Align2::CENTER_CENTER, "tr", FontId::proportional(12.0), INK);
            }
            if c.flags & F_VIB != 0 {
                // width until next column
                let next = m.cols.iter().find(|n| n.tick > c.tick).map(|n| m.x + n.x).unwrap_or(m.x + m.w);
                let (a, b) = (x - 4.0, (next - 4.0).max(x + 14.0));
                let mut pts = Vec::new();
                let mut xx = a;
                let mut k = 0;
                while xx <= b {
                    let yy = oy + if k % 2 == 0 { -2.5 } else { 2.5 };
                    pts.push(self.p(xx, yy));
                    xx += 3.0;
                    k += 1;
                }
                painter.add(Shape::line(pts, Stroke::new(1.3, INK)));
            }
        }
    }

    fn draw_links(&self, painter: &egui::Painter, ri: usize, mi: usize) {
        let lay = self.layout;
        for l in &lay.links[mi] {
            let y = lay.line_y(ri, l.string);
            let xa = lay.x_at(mi, l.from_tick as f64);
            let same_row = lay.meas[l.to_m].row == ri;
            let xb = if same_row { lay.x_at(l.to_m, l.to_tick as f64) } else { xa + 24.0 };
            match l.kind {
                LinkKind::Hammer | LinkKind::Pull => {
                    let (a, b) = (xa + 4.0, xb - 4.0);
                    let top = y - 9.0;
                    let ctrl = [self.p(a, y - 6.0), self.p(a + (b - a) * 0.25, top - 5.0), self.p(a + (b - a) * 0.75, top - 5.0), self.p(b, y - 6.0)];
                    painter.add(CubicBezierShape::from_points_stroke(ctrl, false, Color32::TRANSPARENT, Stroke::new(1.0, INK)));
                    let label = if l.kind == LinkKind::Hammer { "H" } else { "P" };
                    painter.text(self.p((a + b) / 2.0, top - 6.0), Align2::CENTER_BOTTOM, label, FontId::proportional(9.0), INK);
                }
                LinkKind::SlideUp | LinkKind::SlideDown => {
                    let d = if l.kind == LinkKind::SlideUp { 3.5 } else { -3.5 };
                    painter.line_segment([self.p(xa + 8.0, y + d), self.p(xb - 8.0, y - d)], Stroke::new(1.2, INK));
                }
            }
        }
    }

    fn draw_notes(&self, painter: &egui::Painter, ri: usize, mi: usize) {
        let lay = self.layout;
        let m = &lay.meas[mi];
        let track = &self.song.tracks[lay.track];
        let Some(measure) = track.measures.get(mi) else {
            return;
        };
        for (ci, c) in m.cols.iter().enumerate() {
            let x = m.x + c.x;
            let next_x = m.cols.get(ci + 1).map(|n| m.x + n.x).unwrap_or(m.x + m.w);
            for r in &c.notes {
                let beat = &measure.voices[r.voice].beats[r.beat];
                let n = &beat.notes[r.note];
                if lay.drums {
                    self.draw_drum_note(painter, ri, x, n);
                    continue;
                }
                let s = (n.string as usize).saturating_sub(1).min(lay.lines - 1);
                let y = lay.line_y(ri, s);
                let e = &n.effects;
                let (mut text, mut color) = (n.fret.to_string(), INK);
                match n.kind {
                    NoteKind::Dead => text = "x".into(),
                    NoteKind::Tie => {
                        text = format!("({})", n.fret);
                        color = SOFT;
                    }
                    NoteKind::Normal => {}
                }
                if n.ghost && n.kind != NoteKind::Tie {
                    text = format!("({text})");
                    color = SOFT;
                }
                if let Some(hm) = &e.harmonic {
                    text = match hm {
                        Harmonic::Natural => format!("<{text}>"),
                        Harmonic::Artificial(_) | Harmonic::Tapped(_) => format!("{text}<>"),
                        _ => format!("{text}*"),
                    };
                }
                if r.voice > 0 {
                    color = Color32::from_rgb(180, 160, 230);
                }
                self.knockout_text(painter, self.p(x, y), text, 12.5, color);
                if n.accent || n.heavy_accent {
                    let ay = lay.staff_top(ri) - 8.0;
                    painter.text(self.p(x, ay), Align2::CENTER_CENTER, if n.heavy_accent { "^" } else { ">" }, FontId::proportional(11.0), INK);
                }
                if let Some(g) = &e.grace {
                    let gt = if g.dead { "x".to_string() } else { g.fret.to_string() };
                    self.knockout_text(painter, self.p(x - 13.0, y - 1.0), gt, 9.0, INK);
                }
                for sl in &e.slides {
                    let st = Stroke::new(1.2, INK);
                    match sl {
                        Slide::OutDown => { painter.line_segment([self.p(x + 8.0, y - 3.0), self.p(x + 20.0, y + 4.0)], st); }
                        Slide::OutUp => { painter.line_segment([self.p(x + 8.0, y + 3.0), self.p(x + 20.0, y - 4.0)], st); }
                        Slide::InFromBelow => { painter.line_segment([self.p(x - 20.0, y + 4.0), self.p(x - 8.0, y - 3.0)], st); }
                        Slide::InFromAbove => { painter.line_segment([self.p(x - 20.0, y - 4.0), self.p(x - 8.0, y + 3.0)], st); }
                        _ => {}
                    };
                }
                if let Some(b) = &e.bend {
                    self.draw_bend(painter, ri, x, y, (next_x - x).clamp(18.0, 40.0), b);
                }
            }
        }
    }

    fn draw_drum_note(&self, painter: &egui::Painter, ri: usize, x: f32, n: &Note) {
        let lane = drum_lane(n.fret);
        let y = self.layout.line_y(ri, lane);
        let c = if n.ghost { SOFT } else { INK };
        let r = if n.ghost { 3.0 } else { 4.2 };
        if lane <= 2 {
            let st = Stroke::new(1.6, c);
            painter.rect_filled(Rect::from_center_size(self.p(x, y), vec2(10.0, 10.0)), 2.0, BG);
            painter.line_segment([self.p(x - r, y - r), self.p(x + r, y + r)], st);
            painter.line_segment([self.p(x - r, y + r), self.p(x + r, y - r)], st);
            if n.fret == 46 {
                painter.circle_stroke(self.p(x, y - 9.0), 2.5, Stroke::new(1.0, c));
            }
        } else {
            painter.circle_filled(self.p(x, y), r, c);
        }
    }

    fn draw_bend(&self, painter: &egui::Painter, ri: usize, x: f32, y: f32, w: f32, b: &Bend) {
        let lay = self.layout;
        let top = lay.staff_top(ri) - 6.0;
        let max = b.max_value();
        let first = b.points.first().map(|p| p.1).unwrap_or(0.0);
        let last = b.points.last().map(|p| p.1).unwrap_or(0.0);
        let st = Stroke::new(1.2, INK);
        let arrow = |tip: Pos2, dir: egui::Vec2| {
            let d = dir.normalized();
            let n = vec2(-d.y, d.x);
            painter.add(Shape::convex_polygon(vec![tip, tip - d * 6.0 + n * 3.0, tip - d * 6.0 - n * 3.0], INK, Stroke::NONE));
        };
        let x0 = x + 7.0;
        let peak_x;
        if first > 0.1 {
            // pre-bend
            painter.line_segment([self.p(x0, y - 5.0), self.p(x0, top + 2.0)], st);
            arrow(self.p(x0, top), vec2(0.0, -1.0));
            peak_x = x0;
        } else {
            peak_x = x0 + w * 0.55;
            let q = [self.p(x0, y), self.p(peak_x, y), self.p(peak_x, top + 2.0)];
            painter.add(QuadraticBezierShape::from_points_stroke(q, false, Color32::TRANSPARENT, st));
            arrow(self.p(peak_x, top), vec2(0.0, -1.0));
        }
        painter.text(self.p(peak_x, top - 1.0), Align2::CENTER_BOTTOM, bend_label(max), FontId::proportional(10.0), INK);
        if max.abs() > 0.1 && last < max - 0.25 {
            let rx = peak_x + w * 0.45;
            let ry = if last.abs() < 0.1 { y - 5.0 } else { top + 10.0 };
            let q = [self.p(peak_x + 1.0, top + 1.0), self.p(rx, top + 1.0), self.p(rx, ry)];
            painter.add(QuadraticBezierShape::from_points_stroke(q, false, Color32::TRANSPARENT, Stroke::new(1.2, INK)));
            arrow(self.p(rx, ry + 2.0), vec2(0.0, 1.0));
            if last.abs() > 0.1 {
                painter.text(self.p(rx + 3.0, ry), Align2::LEFT_CENTER, bend_label(last), FontId::proportional(9.0), INK);
            }
        }
    }

    fn draw_rhythm(&self, painter: &egui::Painter, ri: usize, mi: usize, bottom: f32) {
        let lay = self.layout;
        let m = &lay.meas[mi];
        let h = &self.song.measure_headers[mi];
        let Some(v) = self.song.tracks[lay.track].measures.get(mi).and_then(|me| me.voices.first()) else {
            return;
        };
        let y0 = bottom + 9.0;
        let y1 = bottom + 27.0;
        let st = Stroke::new(1.1, INK);
        let group_len = (TPQ * 4 / h.denominator.max(1) as u32).max(1);
        // (x, beams, dotted, tuplet)
        let mut group: Vec<(f32, u32, bool, u8)> = Vec::new();
        let mut group_id = u32::MAX;
        let flush = |group: &mut Vec<(f32, u32, bool, u8)>| {
            if group.is_empty() {
                return;
            }
            if group.len() == 1 {
                let (x, beams, _, _) = group[0];
                for k in 0..beams {
                    let y = y1 - k as f32 * 4.0;
                    painter.line_segment([self.p(x, y), self.p(x + 7.0, y - 5.0)], Stroke::new(1.6, INK));
                }
            } else {
                for k in 0..4u32 {
                    let y = y1 - k as f32 * 4.0;
                    let mut i = 0;
                    while i < group.len() {
                        if group[i].1 <= k {
                            i += 1;
                            continue;
                        }
                        let mut j = i;
                        while j + 1 < group.len() && group[j + 1].1 > k {
                            j += 1;
                        }
                        let (xa, xb) = if i == j {
                            if i + 1 < group.len() { (group[i].0, group[i].0 + 6.0) } else { (group[i].0 - 6.0, group[i].0) }
                        } else {
                            (group[i].0, group[j].0)
                        };
                        painter.line_segment([self.p(xa, y), self.p(xb, y)], Stroke::new(2.4, INK));
                        i = j + 1;
                    }
                }
                let tup = group[0].3;
                if tup > 1 && group.iter().all(|g| g.3 == tup) {
                    let xm = (group[0].0 + group[group.len() - 1].0) / 2.0;
                    painter.text(self.p(xm, y1 + 3.0), Align2::CENTER_TOP, tup.to_string(), FontId::proportional(9.5), INK);
                }
            }
            group.clear();
        };
        for b in &v.beats {
            if b.status == BeatStatus::Empty {
                continue;
            }
            let rel = b.start.saturating_sub(h.start);
            let x = lay.x_at(mi, rel as f64);
            let d = b.duration;
            let rest = b.status == BeatStatus::Rest || b.notes.is_empty();
            if rest {
                flush(&mut group);
                group_id = u32::MAX;
                self.draw_rest(painter, ri, x, d.value);
                continue;
            }
            if d.value >= 2 {
                let top = if d.value == 2 { y0 + 8.0 } else { y0 };
                painter.line_segment([self.p(x, top), self.p(x, y1)], st);
            }
            if d.dotted || d.double_dotted {
                painter.circle_filled(self.p(x + 4.0, y1 - 2.0), 1.4, INK);
            }
            let beams = match d.value {
                8 => 1,
                16 => 2,
                32 => 3,
                64 => 4,
                _ => 0,
            };
            if beams == 0 {
                flush(&mut group);
                group_id = u32::MAX;
                continue;
            }
            let gid = rel / group_len;
            if gid != group_id {
                flush(&mut group);
                group_id = gid;
            }
            group.push((x, beams, d.dotted, d.tuplet.0));
        }
        flush(&mut group);
        let _ = m;
    }

    fn draw_rest(&self, painter: &egui::Painter, ri: usize, x: f32, value: u8) {
        let lay = self.layout;
        let mid = (lay.staff_top(ri) + lay.staff_bottom(ri)) / 2.0;
        let c = SOFT;
        let st = Stroke::new(1.5, c);
        painter.rect_filled(Rect::from_center_size(self.p(x, mid), vec2(10.0, 20.0)), 2.0, BG);
        match value {
            1 => {
                painter.rect_filled(Rect::from_min_size(self.p(x - 5.0, mid - 4.0), vec2(10.0, 4.0)), 0.0, c);
            }
            2 => {
                painter.rect_filled(Rect::from_min_size(self.p(x - 5.0, mid), vec2(10.0, 4.0)), 0.0, c);
            }
            4 => {
                let pts = vec![
                    self.p(x - 2.0, mid - 9.0),
                    self.p(x + 3.0, mid - 3.0),
                    self.p(x - 2.0, mid + 2.0),
                    self.p(x + 3.0, mid + 7.0),
                    self.p(x - 1.0, mid + 6.0),
                    self.p(x + 1.0, mid + 10.0),
                ];
                painter.add(Shape::line(pts, st));
            }
            _ => {
                let flags = match value {
                    8 => 1,
                    16 => 2,
                    32 => 3,
                    _ => 4,
                };
                painter.line_segment([self.p(x + 3.0, mid - 7.0), self.p(x - 1.0, mid + 8.0)], st);
                for k in 0..flags {
                    let fy = mid - 6.0 + k as f32 * 4.5;
                    painter.circle_filled(self.p(x - 3.0, fy), 1.8, c);
                    painter.line_segment([self.p(x - 3.0, fy), self.p(x + 3.0 - k as f32 * 1.2, fy - 0.5)], st);
                }
            }
        }
    }
}

pub fn note_name(p: i32) -> String {
    const N: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    N[p.rem_euclid(12) as usize].to_string()
}

/// Paint the tablature and return the response for interaction.
pub fn rows_visible(layout: &TabLayout, viewport: Rect) -> std::ops::Range<usize> {
    let a = layout.rows.partition_point(|r| r.y + r.h < viewport.min.y);
    let b = layout.rows.partition_point(|r| r.y <= viewport.max.y);
    a..b.max(a)
}
