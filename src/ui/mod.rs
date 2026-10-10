//! Main application window.

mod tab;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;
use egui::{Align, Color32, Key, Modifiers, RichText, Sense, Ui, vec2};

use crate::audio::AudioOut;
use crate::model::{Song, ToneKind};
use crate::synth::Command;
use crate::synth::sampler::{BankId, Library, SlotState};
use crate::timeline::{Schedule, build_schedule};
use tab::{Paint, TabLayout};

struct TrackMix {
    mute: bool,
    solo: bool,
    volume: f32,
    tone: ToneKind,
}

struct Loaded {
    path: PathBuf,
    song: Song,
    sched: Arc<Schedule>,
    mix: Vec<TrackMix>,
}

#[derive(Clone, PartialEq)]
enum NamState {
    Unrequested,
    Loading,
    Ready,
    Failed(String),
}

pub struct App {
    loaded: Option<Loaded>,
    audio: Option<AudioOut>,
    audio_error: Option<String>,
    error: Option<String>,
    view_track: usize,
    /// Playback speed relative to the score tempo.
    speed: f64,
    library: Arc<Library>,
    /// Neural Amp Modeler capture found on disk, and its load status.
    nam_path: Option<PathBuf>,
    nam_state: Arc<std::sync::Mutex<NamState>>,
    ctx: egui::Context,
    metronome: bool,
    count_in: bool,
    master: f32,
    selection: Option<(usize, usize)>,
    loop_on: bool,
    drag_anchor: Option<usize>,
    layout: Option<TabLayout>,
    last_cursor_row: Option<usize>,
    was_playing: bool,
    follow: bool,
    scroll_to: Option<f32>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>) -> App {
        cc.egui_ctx.set_visuals(dark_visuals());
        let (audio, audio_error) = match AudioOut::start() {
            Ok(a) => (Some(a), None),
            Err(e) => (None, Some(format!("Audio unavailable: {e:#}"))),
        };
        let mut app = App {
            loaded: None,
            audio,
            audio_error,
            error: None,
            view_track: 0,
            speed: 1.0,
            library: Library::new(),
            nam_path: crate::synth::nam::find_model(),
            nam_state: Arc::new(std::sync::Mutex::new(NamState::Unrequested)),
            ctx: cc.egui_ctx.clone(),
            metronome: false,
            count_in: false,
            master: 0.8,
            selection: None,
            loop_on: false,
            drag_anchor: None,
            layout: None,
            last_cursor_row: None,
            was_playing: false,
            follow: true,
            scroll_to: None,
        };
        if let Some(p) = path {
            app.open(&p);
        }
        app
    }

    fn send(&self, c: Command) {
        if let Some(a) = &self.audio {
            a.send(c);
        }
    }

    /// Start loading the sound bank for a tone in the background (no-op if already requested).
    fn request_bank(&self, tone: ToneKind) {
        let Some(audio) = &self.audio else { return };
        let mut ids: Vec<BankId> = BankId::for_tone(tone).into_iter().collect();
        if tone == ToneKind::Nam {
            // Fallback DI when the humbucker recordings are not installed.
            ids.push(BankId::GuitarDirect);
            self.request_nam();
        }
        for id in ids {
            let tx = audio.tx.clone();
            let ctx = self.ctx.clone();
            self.library.request(id, move |bank| {
                let _ = tx.send(Command::Bank(id, bank));
                ctx.request_repaint();
            });
        }
        self.ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    /// Load the NAM capture in the background (once).
    fn request_nam(&self) {
        let (Some(path), Some(audio)) = (self.nam_path.clone(), &self.audio) else { return };
        {
            let mut st = self.nam_state.lock().unwrap();
            if *st != NamState::Unrequested {
                return;
            }
            *st = NamState::Loading;
        }
        let tx = audio.tx.clone();
        let ctx = self.ctx.clone();
        let state = self.nam_state.clone();
        std::thread::spawn(move || {
            let r = crate::synth::nam::NamModel::load(&path);
            *state.lock().unwrap() = match r {
                Ok(m) => {
                    let _ = tx.send(Command::Nam(Some(Arc::new(m))));
                    NamState::Ready
                }
                Err(e) => NamState::Failed(format!("{e:#}")),
            };
            ctx.request_repaint();
        });
    }

    fn sound_status(&self) -> Option<(String, bool)> {
        let l = self.loaded.as_ref()?;
        let mut loading = Vec::new();
        let mut missing = Vec::new();
        let nam_used = l.mix.iter().any(|m| m.tone == ToneKind::Nam);
        if nam_used {
            match &*self.nam_state.lock().unwrap() {
                NamState::Loading => loading.push("neural amp model"),
                NamState::Failed(e) => return Some((format!("Could not load the NAM capture: {e}"), true)),
                _ if self.nam_path.is_none() => {
                    return Some(("No .nam capture found, Metal (NAM amp) uses the built-in amp sim. Put a .nam file in assets/".into(), true));
                }
                _ => {}
            }
        }
        for m in &l.mix {
            let Some(mut id) = BankId::for_tone(m.tone) else { continue };
            if m.tone == ToneKind::Nam && matches!(self.library.state(id), SlotState::Missing | SlotState::Failed(_)) {
                id = BankId::GuitarDirect;
            }
            match self.library.state(id) {
                SlotState::Loading | SlotState::Unrequested => loading.push(id.label()),
                SlotState::Missing | SlotState::Failed(_) => missing.push(id.label()),
                SlotState::Ready => {}
            }
        }
        loading.dedup();
        missing.dedup();
        if !loading.is_empty() {
            Some((format!("Loading sounds: {}…", loading.join(", ")), false))
        } else if !missing.is_empty() {
            Some((format!("Sound bank not found ({}), using built-in synth. Run scripts/fetch-sounds.sh", missing.join(", ")), true))
        } else {
            None
        }
    }

    fn orig_bpm(&self) -> f64 {
        self.loaded.as_ref().map(|l| l.song.tempo as f64).filter(|&t| t > 0.0).unwrap_or(120.0)
    }

    fn open(&mut self, path: &Path) {
        match crate::gp::load_file(path) {
            Ok(song) => {
                let mix: Vec<TrackMix> = song
                    .tracks
                    .iter()
                    .map(|t| TrackMix { mute: false, solo: false, volume: t.volume as f32 / 127.0, tone: t.default_tone_with_nam(self.nam_path.is_some()) })
                    .collect();
                let tones: Vec<ToneKind> = mix.iter().map(|m| m.tone).collect();
                let sched = Arc::new(build_schedule(&song, &tones));
                self.send(Command::Pause);
                self.send(Command::SetSchedule(sched.clone()));
                self.send(Command::Seek(0));
                for (i, m) in mix.iter().enumerate() {
                    self.send(Command::Mute(i, false));
                    self.send(Command::Solo(i, false));
                    self.send(Command::Volume(i, m.volume));
                }
                self.send(Command::Loop(None));
                for m in &mix {
                    self.request_bank(m.tone);
                }
                // Default view: first non-drum track.
                self.view_track = song.tracks.iter().position(|t| !t.percussion).unwrap_or(0);
                self.loaded = Some(Loaded { path: path.to_path_buf(), song, sched, mix });
                self.layout = None;
                self.selection = None;
                self.loop_on = false;
                self.scroll_to = Some(0.0);
                self.error = None;
            }
            Err(e) => self.error = Some(format!("Could not open {}: {e:#}", path.display())),
        }
    }

    fn open_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Guitar Pro", &["gp3", "gp4", "gp5", "gpx"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open(&p);
        }
    }

    fn pos(&self) -> f64 {
        self.audio.as_ref().map(|a| a.shared.pos()).unwrap_or(0.0)
    }

    fn playing(&self) -> bool {
        self.audio.as_ref().map(|a| a.shared.playing.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(false)
    }

    fn toggle_play(&mut self) {
        if self.playing() {
            self.send(Command::Pause);
        } else {
            self.send(Command::Play);
        }
    }

    fn set_bpm(&mut self, bpm: f64) {
        let orig = self.orig_bpm();
        let bpm = bpm.round().clamp((orig * 0.1).ceil(), (orig * 2.0).floor());
        self.speed = bpm / orig;
        self.send(Command::Speed(self.speed));
    }

    fn target_bpm(&self) -> f64 {
        (self.orig_bpm() * self.speed).round()
    }

    /// Playback tick for a score measure + offset, choosing the repeat pass nearest the cursor.
    fn tick_for(&self, measure: usize, rel: u32) -> Option<u64> {
        let l = self.loaded.as_ref()?;
        let pm = l.sched.timeline.occurrence(measure, self.pos() as u64)?;
        Some(pm.start + rel as u64)
    }

    fn seek_measure(&mut self, measure: usize) {
        if let Some(t) = self.tick_for(measure, 0) {
            self.send(Command::Seek(t));
        }
    }

    fn current_measure(&self) -> Option<(usize, f64)> {
        let l = self.loaded.as_ref()?;
        let pos = self.pos();
        let tl = &l.sched.timeline;
        let k = tl.locate(pos as u64)?;
        let pm = tl.measures[k];
        Some((pm.index, (pos - pm.start as f64).max(0.0)))
    }

    fn apply_loop(&mut self) {
        let range = if self.loop_on {
            self.selection.and_then(|(a, b)| {
                let l = self.loaded.as_ref()?;
                let tl = &l.sched.timeline;
                let pa = tl.occurrence(a, self.pos() as u64)?;
                let pb = tl.occurrence(b, pa.start)?;
                let (s, e) = if pb.start >= pa.start { (pa.start, pb.start + pb.len as u64) } else { (pa.start, pa.start + pa.len as u64) };
                Some((s, e))
            })
        } else {
            None
        };
        self.send(Command::Loop(range));
        if let Some((s, _)) = range {
            let p = self.pos() as u64;
            if p < s || range.is_some_and(|(_, e)| p >= e) {
                self.send(Command::Seek(s));
            }
        }
    }

    fn handle_keys(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let mut toggle = false;
        let mut speed_delta: i32 = 0;
        let mut measure_delta: i32 = 0;
        let mut home = false;
        let mut open = false;
        let mut clear_loop = false;
        let mut loop_current = false;
        let mut metronome = false;
        let mut track_key = None;
        ctx.input_mut(|i| {
            // Consume Space so focused buttons don't also react.
            toggle = i.consume_key(Modifiers::NONE, Key::Space);
            if i.consume_key(Modifiers::NONE, Key::Plus) || i.consume_key(Modifiers::NONE, Key::Equals) || i.consume_key(Modifiers::SHIFT, Key::Equals) {
                speed_delta = 5;
            }
            if i.consume_key(Modifiers::NONE, Key::Minus) {
                speed_delta = -5;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowRight) {
                measure_delta = 1;
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowLeft) {
                measure_delta = -1;
            }
            home = i.consume_key(Modifiers::NONE, Key::Home);
            open = i.consume_key(Modifiers::COMMAND, Key::O);
            clear_loop = i.consume_key(Modifiers::NONE, Key::Escape);
            loop_current = i.consume_key(Modifiers::NONE, Key::L);
            metronome = i.consume_key(Modifiers::NONE, Key::M);
            let nums = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
            for (k, key) in nums.iter().enumerate() {
                if i.consume_key(Modifiers::NONE, *key) {
                    track_key = Some(k);
                }
            }
        });
        if open {
            self.open_dialog();
        }
        if self.loaded.is_none() {
            return;
        }
        if toggle {
            self.toggle_play();
        }
        if speed_delta != 0 {
            self.set_bpm(self.target_bpm() + speed_delta as f64);
        }
        if measure_delta != 0 {
            if let Some((m, _)) = self.current_measure() {
                let n = self.loaded.as_ref().unwrap().song.measure_headers.len() as i32;
                let target = (m as i32 + measure_delta).clamp(0, n - 1) as usize;
                self.seek_measure(target);
                self.follow = true;
            }
        }
        if home {
            let start = self.selection.filter(|_| self.loop_on).and_then(|(a, _)| self.tick_for(a, 0)).unwrap_or(0);
            self.send(Command::Seek(start));
            self.scroll_to = if start == 0 { Some(0.0) } else { None };
            self.follow = true;
        }
        if clear_loop {
            self.loop_on = false;
            self.selection = None;
            self.apply_loop();
        }
        if loop_current {
            if self.loop_on {
                self.loop_on = false;
            } else {
                if self.selection.is_none() {
                    self.selection = self.current_measure().map(|(m, _)| (m, m));
                }
                self.loop_on = self.selection.is_some();
            }
            self.apply_loop();
        }
        if metronome {
            self.metronome = !self.metronome;
            self.send(Command::Metronome(self.metronome));
        }
        if let Some(k) = track_key {
            if k < self.loaded.as_ref().unwrap().song.tracks.len() {
                self.view_track = k;
            }
        }
    }

    fn toolbar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if ui.button("📂 Open…").on_hover_text("Open a Guitar Pro file (Ctrl+O)").clicked() {
                self.open_dialog();
            }
            ui.separator();
            let playing = self.playing();
            let enabled = self.loaded.is_some() && self.audio.is_some();
            let label = if playing { "⏸ Pause" } else { "▶ Play" };
            if ui
                .add_enabled(enabled, egui::Button::new(RichText::new(label).size(16.0)).min_size(vec2(90.0, 28.0)))
                .on_hover_text("Space")
                .clicked()
            {
                self.toggle_play();
            }
            if ui.add_enabled(enabled, egui::Button::new("⏮")).on_hover_text("Back to start (Home)").clicked() {
                let start = self.selection.filter(|_| self.loop_on).and_then(|(a, _)| self.tick_for(a, 0)).unwrap_or(0);
                self.send(Command::Seek(start));
                self.follow = true;
            }
            ui.separator();
            ui.label("Tempo");
            let orig = self.orig_bpm();
            let mut bpm = self.target_bpm();
            let lo = (orig * 0.25).round().max(10.0);
            let hi = (orig * 1.5).round();
            let resp = ui.add_enabled(
                self.loaded.is_some(),
                egui::Slider::new(&mut bpm, lo..=hi).suffix(" BPM").step_by(1.0).max_decimals(0),
            );
            if resp.on_hover_text("Playback tempo (+ / − keys)").changed() {
                self.set_bpm(bpm);
            }
            let changed = (self.speed - 1.0).abs() > 1e-6;
            let orig_text = RichText::new(format!("Original {orig:.0}"));
            ui.label(if changed { orig_text.color(ui.visuals().warn_fg_color) } else { orig_text.weak() });
            if ui
                .add_enabled(changed, egui::Button::new("↺ Reset"))
                .on_hover_text(format!("Back to the original {orig:.0} BPM"))
                .clicked()
            {
                self.set_bpm(orig);
            }
            ui.separator();
            if ui.toggle_value(&mut self.metronome, "🕑 Metronome").on_hover_text("M").changed() {
                self.send(Command::Metronome(self.metronome));
            }
            if ui.toggle_value(&mut self.count_in, "Count-in").changed() {
                self.send(Command::CountIn(self.count_in));
            }
            let loop_resp = ui
                .add_enabled(self.selection.is_some() || self.loop_on, egui::Button::selectable(self.loop_on, "🔁 Loop"))
                .on_hover_text("Drag across measures to select, then L to loop. Esc clears.");
            if loop_resp.clicked() {
                self.loop_on = !self.loop_on;
                self.apply_loop();
            }
            ui.separator();
            ui.label("🔊");
            if ui.add(egui::Slider::new(&mut self.master, 0.0..=1.2).show_value(false)).changed() {
                self.send(Command::MasterVolume(self.master));
            }
        });
    }

    fn status(&self, ui: &mut Ui) {
        let Some(l) = &self.loaded else { return };
        let tl = &l.sched.timeline;
        let pos = self.pos();
        let speed = self.speed;
        let t = tl.seconds_at(pos) / speed;
        let total = tl.seconds_at(tl.total as f64) / speed;
        let fmt = |s: f64| format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32);
        let bar = self.current_measure().map(|(m, _)| m + 1).unwrap_or(1);
        let bpm = tl.bpm_at(pos as u64);
        ui.horizontal(|ui| {
            ui.monospace(format!("{} / {}", fmt(t), fmt(total)));
            ui.separator();
            ui.label(format!("Bar {bar} / {}", l.song.measure_headers.len()));
            ui.separator();
            ui.label(format!("♩ = {:.0}", bpm * speed));
            if (speed - 1.0).abs() > 1e-6 {
                ui.label(RichText::new(format!("(score {:.0}, {:.0}%)", bpm, speed * 100.0)).weak());
            }
            if self.loop_on {
                if let Some((a, b)) = self.selection {
                    ui.separator();
                    ui.label(RichText::new(format!("Looping bars {}–{}", a + 1, b + 1)).color(Color32::from_rgb(110, 170, 255)));
                }
            }
            if let Some((msg, warn)) = self.sound_status() {
                ui.separator();
                if warn {
                    ui.colored_label(ui.visuals().warn_fg_color, msg);
                } else {
                    ui.spinner();
                    ui.label(RichText::new(msg).weak());
                }
            }
            if let Some(e) = &self.audio_error {
                ui.separator();
                ui.colored_label(Color32::RED, e);
            }
        });
    }

    fn track_panel(&mut self, ui: &mut Ui) {
        let Some(l) = &mut self.loaded else { return };
        let mut cmds = Vec::new();
        ui.add_space(6.0);
        ui.heading(RichText::new(&l.song.title).strong());
        if !l.song.artist.is_empty() {
            ui.label(RichText::new(&l.song.artist).weak());
        }
        ui.add_space(8.0);
        ui.label(RichText::new("TRACKS").small().weak());
        let any_solo = l.mix.iter().any(|m| m.solo);
        for (i, t) in l.song.tracks.iter().enumerate() {
            let m = &mut l.mix[i];
            let selected = self.view_track == i;
            let frame = egui::Frame::new()
                .fill(if selected { Color32::from_rgb(44, 52, 66) } else { Color32::TRANSPARENT })
                .corner_radius(6.0)
                .inner_margin(6.0);
            let resp = frame
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (r, g, b) = t.color;
                        let (rect, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                        ui.painter().circle_filled(rect.center(), 5.0, Color32::from_rgb(r, g, b));
                        let audible = !m.mute && (!any_solo || m.solo);
                        let name = RichText::new(if t.name.trim().is_empty() { t.instrument_name() } else { t.name.trim() }).strong();
                        ui.label(if audible { name } else { name.weak() });
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(egui::Button::selectable(m.solo, "S")).on_hover_text("Solo").clicked() {
                                m.solo = !m.solo;
                                cmds.push(Command::Solo(i, m.solo));
                            }
                            if ui.add(egui::Button::selectable(m.mute, "M")).on_hover_text("Mute").clicked() {
                                m.mute = !m.mute;
                                cmds.push(Command::Mute(i, m.mute));
                            }
                        });
                    });
                    let sub = if t.percussion {
                        "Drums".to_string()
                    } else {
                        format!("{} · {}", t.instrument_name(), t.tuning_name())
                    };
                    ui.label(RichText::new(sub).small().weak());
                    if selected {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Vol").small());
                            if ui.add(egui::Slider::new(&mut m.volume, 0.0..=1.0).show_value(false)).changed() {
                                cmds.push(Command::Volume(i, m.volume));
                            }
                        });
                        if !t.percussion {
                                let choices: &[ToneKind] = if m.tone.is_bass() { &ToneKind::ALL_BASS } else { &ToneKind::ALL_GUITAR };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Tone").small());
                                egui::ComboBox::from_id_salt(("tone", i)).selected_text(m.tone.label()).show_ui(ui, |ui| {
                                    for &k in choices {
                                        if ui.selectable_value(&mut m.tone, k, k.label()).changed() {
                                            cmds.push(Command::Tone(i, k));
                                        }
                                    }
                                });
                            });
                        }
                    }
                })
                .response;
            let click = ui.interact(resp.rect, ui.id().with(("track", i)), Sense::click());
            if click.clicked() {
                self.view_track = i;
            }
            if i < 9 {
                click.on_hover_text(format!("Show this track (key {})", i + 1));
            }
            ui.add_space(2.0);
        }
        ui.add_space(12.0);
        ui.separator();
        ui.label(RichText::new("SHORTCUTS").small().weak());
        for (k, d) in [
            ("Space", "play / pause"),
            ("Click", "move cursor"),
            ("Drag", "select bars"),
            ("L / Esc", "loop / clear"),
            ("← →", "prev / next bar"),
            ("+ / −", "tempo ±5 BPM"),
            ("M", "metronome"),
            ("1–9", "switch track"),
        ] {
            ui.horizontal(|ui| {
                ui.label(RichText::new(k).small().monospace());
                ui.label(RichText::new(d).small().weak());
            });
        }
        if let Some(p) = l.path.file_name() {
            ui.add_space(8.0);
            ui.label(RichText::new(p.to_string_lossy()).small().weak());
        }
        for c in cmds {
            if let Command::Tone(_, k) = c {
                self.request_bank(k);
            }
            self.send(c);
        }
    }

    fn tab_view(&mut self, ui: &mut Ui) {
        let Some(l) = &self.loaded else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Open a Guitar Pro file (.gp3 / .gp4 / .gp5) or drop one here").size(18.0).weak());
            });
            return;
        };
        let width = ui.available_width();
        let rebuild = match &self.layout {
            Some(lay) => lay.track != self.view_track || (lay.width - width).abs() > 1.0,
            None => true,
        };
        if rebuild {
            self.layout = Some(TabLayout::build(&l.song, self.view_track, width));
            self.last_cursor_row = None;
        }
        let layout = self.layout.as_ref().unwrap();
        let cursor = self.current_measure();
        let playing = self.playing();
        if playing && !self.was_playing {
            self.follow = true;
        }
        self.was_playing = playing;

        let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]).id_salt("tab");
        if let Some(y) = self.scroll_to.take() {
            scroll = scroll.vertical_scroll_offset(y);
        }
        let mut action: Option<TabAction> = None;
        let cursor_row = cursor.and_then(|(m, _)| layout.meas.get(m)).map(|m| m.row);
        let out = scroll.show_viewport(ui, |ui, viewport| {
            let origin = ui.max_rect().min;
            let rect = egui::Rect::from_min_size(origin, vec2(width, layout.height));
            let resp = ui.allocate_rect(rect, Sense::click_and_drag());
            ui.painter().rect_filled(rect, 0.0, tab::BG);
            let paint = Paint {
                song: &l.song,
                layout,
                origin,
                viewport,
                cursor,
                selection: self.selection,
                loop_active: self.loop_on,
            };
            paint.draw(ui);

            // Follow the cursor while playing.
            if let Some(row) = cursor_row {
                if self.follow && Some(row) != self.last_cursor_row {
                    let r = layout.row_rect(row).translate(origin.to_vec2());
                    let target = egui::Rect::from_min_max(r.min - vec2(0.0, r.height() * 0.5), r.max);
                    let fully_visible = viewport.contains_rect(layout.row_rect(row));
                    if !fully_visible || playing {
                        ui.scroll_to_rect(target, Some(Align::Min));
                    }
                    self.last_cursor_row = Some(row);
                }
            }

            let hit = |p: egui::Pos2| layout.hit(p - origin.to_vec2());
            if resp.drag_started() {
                if let Some(p) = resp.interact_pointer_pos() {
                    action = hit(p).map(|(m, _)| TabAction::DragStart(m));
                }
            } else if resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    action = hit(p).map(|(m, _)| TabAction::DragTo(m));
                }
            } else if resp.drag_stopped() {
                action = Some(TabAction::DragEnd);
            }
            if resp.clicked() {
                if let Some(p) = resp.interact_pointer_pos() {
                    action = hit(p).map(|(m, t)| TabAction::Click(m, t));
                }
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        });
        // Scrolling by hand stops auto-follow until playback restarts or the cursor is moved.
        if ui.rect_contains_pointer(out.inner_rect) && ui.input(|i| i.smooth_scroll_delta.y.abs() > 0.5) {
            self.follow = false;
        }

        match action {
            Some(TabAction::Click(m, t)) => {
                if let Some(tick) = self.tick_for(m, t) {
                    self.send(Command::Seek(tick));
                }
                if self.loop_on && self.selection.is_some_and(|(a, b)| m < a || m > b) {
                    self.loop_on = false;
                    self.selection = None;
                    self.apply_loop();
                } else if !self.loop_on {
                    self.selection = None;
                }
                self.follow = true;
                self.last_cursor_row = cursor_row;
            }
            Some(TabAction::DragStart(m)) => {
                self.drag_anchor = Some(m);
                self.selection = Some((m, m));
            }
            Some(TabAction::DragTo(m)) => {
                if let Some(a) = self.drag_anchor {
                    self.selection = Some((a.min(m), a.max(m)));
                }
            }
            Some(TabAction::DragEnd) => {
                self.drag_anchor = None;
                if let Some((a, _)) = self.selection {
                    // Selecting bars turns on looping (as on Songsterr).
                    self.loop_on = true;
                    self.apply_loop();
                    if let Some(t) = self.tick_for(a, 0) {
                        self.send(Command::Seek(t));
                    }
                }
            }
            None => {}
        }
    }
}

enum TabAction {
    Click(usize, u32),
    DragStart(usize),
    DragTo(usize),
    DragEnd,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if let Some(p) = dropped.first() {
            self.open(p);
        }
        self.handle_keys(ui);

        let title = match &self.loaded {
            Some(l) if !l.song.title.is_empty() => format!("{} – {} · Guitar Practice", l.song.artist, l.song.title),
            _ => "Guitar Practice".to_string(),
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(title));

        egui::Panel::top("toolbar").show(ui, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(2.0);
            self.status(ui);
            if let Some(e) = &self.error {
                ui.colored_label(Color32::RED, e);
            }
        });
        if self.loaded.is_some() {
            egui::Panel::left("tracks").default_size(230.0).resizable(true).show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.track_panel(ui));
            });
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(tab::BG)).show(ui, |ui| {
            self.tab_view(ui);
        });

        if self.playing() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

/// Dark theme tuned for long practice sessions.
fn dark_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::from_rgb(28, 31, 36);
    v.window_fill = Color32::from_rgb(32, 35, 41);
    v.extreme_bg_color = Color32::from_rgb(20, 22, 26);
    v.faint_bg_color = Color32::from_rgb(36, 40, 46);
    v.selection.bg_fill = Color32::from_rgb(52, 92, 150);
    v.hyperlink_color = Color32::from_rgb(110, 170, 255);
    v
}
