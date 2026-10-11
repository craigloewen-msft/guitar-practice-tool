//! Main application window.

#[cfg(test)]
pub(crate) mod screenshots;
mod tab;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eframe::egui;
use egui::{Align, Color32, CornerRadius, Key, Margin, Modifiers, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};

use crate::audio::AudioOut;
use crate::model::{Song, ToneKind};
use crate::setup::{self, Settings, SharedProgress};
use crate::synth::Command;
use crate::synth::sampler::{BankId, Library, SlotState};
use crate::timeline::{Schedule, build_schedule};
use tab::{Paint, TabLayout};
use theme::{ACCENT, BG, BLUE, BORDER, CARD, CARD_HOVER, DANGER, GOOD, MUTED, SURFACE, TEXT, WARN};

pub const APP_NAME: &str = "Guitar Practice Tool";
const ICON_PNG: &[u8] = include_bytes!("../../packaging/icons/icon-256.png");
const TONE3000_URL: &str = "https://www.tone3000.com/search?gears=full-rig%2Camp&sort=downloads-all-time";

pub fn icon() -> egui::IconData {
    eframe::icon_data::from_png_bytes(ICON_PNG).unwrap_or_default()
}

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
    /// Neural Amp Modeler capture in use, and its load status.
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
    settings: Settings,
    settings_dirty: bool,
    sounds_ok: bool,
    download: Option<SharedProgress>,
    show_setup: bool,
    icon: egui::TextureHandle,
    /// Track card rects from the previous frame (click targets behind the card contents).
    track_rects: Vec<Rect>,
    title: String,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, path: Option<PathBuf>) -> App {
        theme::apply(&cc.egui_ctx);
        let (audio, audio_error) = match AudioOut::start() {
            Ok(a) => (Some(a), None),
            Err(e) => (None, Some(format!("No audio output: {e:#}"))),
        };
        let settings = Settings::load();
        let library = Library::new();
        let ic = icon();
        let icon = cc.egui_ctx.load_texture(
            "app-icon",
            egui::ColorImage::from_rgba_unmultiplied([ic.width as usize, ic.height as usize], &ic.rgba),
            egui::TextureOptions::LINEAR,
        );
        let mut app = App {
            loaded: None,
            audio,
            audio_error,
            error: None,
            view_track: 0,
            speed: 1.0,
            sounds_ok: setup::sounds_installed(&library),
            library,
            nam_path: crate::synth::nam::find_model(),
            nam_state: Arc::new(std::sync::Mutex::new(NamState::Unrequested)),
            ctx: cc.egui_ctx.clone(),
            metronome: settings.metronome,
            count_in: settings.count_in,
            master: settings.master,
            selection: None,
            loop_on: false,
            drag_anchor: None,
            layout: None,
            last_cursor_row: None,
            was_playing: false,
            follow: true,
            scroll_to: None,
            settings,
            settings_dirty: false,
            download: None,
            show_setup: false,
            icon,
            track_rects: Vec::new(),
            title: String::new(),
        };
        app.send(Command::MasterVolume(app.master));
        app.send(Command::Metronome(app.metronome));
        app.send(Command::CountIn(app.count_in));
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
        if matches!(tone, ToneKind::Nam | ToneKind::Clean) {
            self.request_nam();
        }
        if let Some(id) = BankId::for_tone(tone) {
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

    /// Sounds still loading in the background, for the spinner in the transport bar.
    fn loading_sounds(&self) -> Option<String> {
        let l = self.loaded.as_ref()?;
        let mut loading = Vec::new();
        if l.mix.iter().any(|m| m.tone == ToneKind::Nam) && *self.nam_state.lock().unwrap() == NamState::Loading {
            loading.push("amp model");
        }
        for m in &l.mix {
            let Some(id) = BankId::for_tone(m.tone) else { continue };
            if matches!(self.library.state(id), SlotState::Loading) {
                loading.push(id.label());
            }
        }
        loading.dedup();
        (!loading.is_empty()).then(|| format!("Loading {}…", loading.join(", ")))
    }

    fn uses_nam(&self) -> bool {
        self.loaded.as_ref().is_some_and(|l| l.mix.iter().any(|m| m.tone == ToneKind::Nam))
    }

    fn start_download(&mut self) {
        if self.download.is_some() {
            return;
        }
        let ctx = self.ctx.clone();
        let p = setup::start_download(self.library.clone(), move || ctx.request_repaint());
        self.download = Some(p);
    }

    /// Called every frame: pick up a finished download and load the new sounds.
    fn poll_download(&mut self) {
        let Some(p) = &self.download else { return };
        let Some(result) = p.lock().unwrap().finished.clone() else {
            self.ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        };
        self.download = None;
        self.sounds_ok = setup::sounds_installed(&self.library);
        match result {
            Ok(()) => {
                let tones: Vec<ToneKind> = self.loaded.iter().flat_map(|l| l.mix.iter().map(|m| m.tone)).collect();
                for t in tones {
                    self.request_bank(t);
                }
            }
            Err(e) => self.error = Some(format!("Download failed: {e}")),
        }
    }

    fn choose_nam(&mut self) {
        let Some(src) = rfd::FileDialog::new().add_filter("Neural Amp Modeler capture", &["nam"]).pick_file() else { return };
        let r = crate::synth::nam::NamModel::load(&src).and_then(|_| setup::install_nam_model(&src));
        match r {
            Ok(dest) => {
                self.settings.nam_model = Some(dest.clone());
                self.settings.save();
                self.nam_path = Some(dest);
                *self.nam_state.lock().unwrap() = NamState::Unrequested;
                if self.uses_nam() {
                    self.request_nam();
                }
                self.error = None;
            }
            Err(e) => self.error = Some(format!("That .nam file could not be used: {e:#}")),
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
                    .map(|t| TrackMix { mute: false, solo: false, volume: t.volume as f32 / 127.0, tone: t.default_tone() })
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
                self.settings.push_recent(path);
                self.settings.save();
            }
            Err(e) => self.error = Some(format!("Could not open {}: {e:#}", path.display())),
        }
    }

    fn open_dialog(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .add_filter("Guitar Pro", &["gp3", "gp4", "gp5"])
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

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add(egui::Image::new(&self.icon).fit_to_exact_size(vec2(30.0, 30.0)));
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                match &self.loaded {
                    Some(l) => {
                        let title = if l.song.title.trim().is_empty() {
                            l.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
                        } else {
                            l.song.title.clone()
                        };
                        ui.label(RichText::new(title).size(16.0).strong().color(TEXT));
                        let sub = if l.song.artist.trim().is_empty() { APP_NAME.to_string() } else { l.song.artist.clone() };
                        ui.label(RichText::new(sub).color(MUTED));
                    }
                    None => {
                        ui.add_space(6.0);
                        ui.label(RichText::new(APP_NAME).size(16.0).strong().color(TEXT));
                    }
                }
            });
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button("⚙  Sounds").on_hover_text("Sound downloads and amp model").clicked() {
                    self.show_setup = true;
                }
                if ui.button("📂  Open…").on_hover_text("Open a Guitar Pro file (Ctrl+O)").clicked() {
                    self.open_dialog();
                }
                if let Some(msg) = self.loading_sounds() {
                    ui.label(RichText::new(msg).color(MUTED));
                    ui.spinner();
                }
            });
        });
    }

    /// One-line notices with a fix-it button, shown above the tab.
    fn banners(&mut self, ui: &mut Ui) {
        let mut notices: Vec<(Color32, String, Option<(&str, u8)>)> = Vec::new();
        if let Some(e) = &self.audio_error {
            notices.push((DANGER, e.clone(), None));
        }
        if let Some(e) = &self.error {
            notices.push((DANGER, e.clone(), Some(("Dismiss", 0))));
        }
        if !self.sounds_ok && self.loaded.is_some() {
            let msg = if self.download.is_some() {
                "Downloading sounds… playback uses the built-in synth until it finishes.".to_string()
            } else {
                "Realistic guitar, bass and drum sounds are not installed yet.".to_string()
            };
            notices.push((WARN, msg, self.download.is_none().then_some(("Download sounds", 1))));
        }
        if self.uses_nam() {
            if let NamState::Failed(e) = &*self.nam_state.lock().unwrap() {
                notices.push((DANGER, format!("Amp model failed to load: {e}"), Some(("Choose another…", 2))));
            } else if self.nam_path.is_none() {
                notices.push((WARN, "No amp model chosen; distorted parts use a basic built-in amp.".into(), Some(("Choose .nam…", 2))));
            }
        }
        for (color, msg, action) in notices {
            let fill = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 22);
            egui::Frame::new()
                .fill(fill)
                .stroke(Stroke::new(1.0, color.gamma_multiply(0.35)))
                .corner_radius(CornerRadius::same(theme::RADIUS))
                .inner_margin(Margin::symmetric(12, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("●").color(color));
                        ui.label(RichText::new(msg).color(TEXT));
                        if let Some((label, id)) = action {
                            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button(label).clicked() {
                                    match id {
                                        0 => self.error = None,
                                        1 => self.start_download(),
                                        _ => self.choose_nam(),
                                    }
                                }
                            });
                        }
                    });
                });
            ui.add_space(4.0);
        }
    }

    fn transport(&mut self, ui: &mut Ui) {
        let Some(l) = &self.loaded else { return };
        let tl = &l.sched.timeline;
        let total_ticks = tl.total.max(1) as f64;
        let pos = self.pos();
        let speed = self.speed;
        let fmt = |s: f64| format!("{}:{:02}", (s / 60.0) as u32, (s % 60.0) as u32);
        let now = fmt(tl.seconds_at(pos) / speed);
        let total = fmt(tl.seconds_at(total_ticks) / speed);
        let loop_span = if self.loop_on {
            self.selection.and_then(|(a, b)| {
                let pa = tl.occurrence(a, pos as u64)?;
                let pb = tl.occurrence(b, pa.start)?;
                Some((pa.start as f64, (pb.start + pb.len as u64).max(pa.start + pa.len as u64) as f64))
            })
        } else {
            None
        };

        // Progress strip
        let mut seek_to = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(now).monospace().color(MUTED));
            let w = ui.available_width() - 52.0;
            let (rect, resp) = ui.allocate_exact_size(vec2(w.max(50.0), 18.0), Sense::click_and_drag());
            let hot = resp.hovered() || resp.dragged();
            let bar = Rect::from_center_size(rect.center(), vec2(rect.width(), if hot { 6.0 } else { 4.0 }));
            let p = ui.painter();
            p.rect_filled(bar, 3.0, BORDER);
            let x_of = |t: f64| bar.left() + bar.width() * (t / total_ticks).clamp(0.0, 1.0) as f32;
            if let Some((a, b)) = loop_span {
                p.rect_filled(Rect::from_x_y_ranges(x_of(a)..=x_of(b), bar.y_range()).expand2(vec2(0.0, 2.0)), 3.0, BLUE.gamma_multiply(0.45));
            }
            let x = x_of(pos);
            p.rect_filled(Rect::from_min_max(bar.min, pos2(x, bar.max.y)), 3.0, ACCENT);
            p.circle_filled(pos2(x, bar.center().y), if hot { 7.0 } else { 5.0 }, if hot { TEXT } else { ACCENT });
            let tick_at = |px: f32| (((px - bar.left()) / bar.width()).clamp(0.0, 1.0) as f64 * total_ticks) as u64;
            if let Some(hp) = resp.hover_pos() {
                if let Some(k) = tl.locate(tick_at(hp.x)) {
                    resp.clone().on_hover_text_at_pointer(format!("Bar {}", tl.measures[k].index + 1));
                }
            }
            if resp.clicked() || resp.dragged() {
                if let Some(pp) = resp.interact_pointer_pos() {
                    seek_to = Some(tick_at(pp.x));
                }
            }
            resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            ui.label(RichText::new(total).monospace().color(MUTED));
        });
        if let Some(t) = seek_to {
            self.send(Command::Seek(t));
            self.follow = true;
        }

        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let enabled = self.audio.is_some();
            if ui.add_enabled(enabled, egui::Button::new(RichText::new("⏮").size(16.0)).min_size(vec2(36.0, 36.0)).corner_radius(18.0))
                .on_hover_text("Back to start (Home)")
                .clicked()
            {
                let start = self.selection.filter(|_| self.loop_on).and_then(|(a, _)| self.tick_for(a, 0)).unwrap_or(0);
                self.send(Command::Seek(start));
                self.follow = true;
            }
            if play_button(ui, self.playing(), enabled).on_hover_text("Play / pause (Space)").clicked() {
                self.toggle_play();
            }
            ui.add_space(8.0);
            let bar = self.current_measure().map(|(m, _)| m + 1).unwrap_or(1);
            let n = self.loaded.as_ref().map(|l| l.song.measure_headers.len()).unwrap_or(0);
            stat(ui, "BAR", &format!("{bar} / {n}"));
            let score_bpm = self.loaded.as_ref().map(|l| l.sched.timeline.bpm_at(self.pos() as u64)).unwrap_or(120.0);
            stat(ui, "NOW", &format!("♩ {:.0}", score_bpm * self.speed));

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().slider_width = 90.0;
                let vol = ui.add(egui::Slider::new(&mut self.master, 0.0..=1.2).show_value(false)).on_hover_text("Master volume");
                if vol.changed() {
                    self.send(Command::MasterVolume(self.master));
                    self.settings_dirty = true;
                }
                ui.label(RichText::new("🔊").color(MUTED));
                ui.add_space(10.0);

                let loop_resp = ui
                    .add_enabled(self.selection.is_some() || self.loop_on, toggle_chip(self.loop_on, "🔁 Loop"))
                    .on_hover_text("Drag across bars in the tab to select them, then L to loop. Esc clears.");
                if loop_resp.clicked() {
                    self.loop_on = !self.loop_on;
                    self.apply_loop();
                }
                if ui.add(toggle_chip(self.count_in, "Count-in")).on_hover_text("One bar of clicks before playback").clicked() {
                    self.count_in = !self.count_in;
                    self.send(Command::CountIn(self.count_in));
                    self.settings_dirty = true;
                }
                if ui.add(toggle_chip(self.metronome, "🕑 Click")).on_hover_text("Metronome (M)").clicked() {
                    self.metronome = !self.metronome;
                    self.send(Command::Metronome(self.metronome));
                    self.settings_dirty = true;
                }
                ui.add_space(10.0);
                self.tempo_control(ui);
            });
        });
    }

    /// BPM slider with −/+ buttons and the score tempo for reference.
    fn tempo_control(&mut self, ui: &mut Ui) {
        let orig = self.orig_bpm();
        let changed = (self.speed - 1.0).abs() > 1e-6;
        // Right-to-left layout: widgets appear in reverse order.
        if changed {
            if ui.small_button(format!("↺ {orig:.0}")).on_hover_text(format!("Reset to the original {orig:.0} BPM")).clicked() {
                self.set_bpm(orig);
            }
        } else {
            ui.label(RichText::new("original").small().color(MUTED));
        }
        if ui.small_button("+").on_hover_text("+5 BPM (+)").clicked() {
            self.set_bpm(self.target_bpm() + 5.0);
        }
        let mut bpm = self.target_bpm();
        let lo = (orig * 0.25).round().max(10.0);
        let hi = (orig * 1.5).round();
        ui.spacing_mut().slider_width = 150.0;
        let s = egui::Slider::new(&mut bpm, lo..=hi).step_by(1.0).show_value(false);
        if ui.add(s).on_hover_text("Playback tempo").changed() {
            self.set_bpm(bpm);
        }
        if ui.small_button("−").on_hover_text("−5 BPM (−)").clicked() {
            self.set_bpm(self.target_bpm() - 5.0);
        }
        let color = if changed { ACCENT } else { TEXT };
        ui.label(RichText::new(format!("{:.0} BPM", self.target_bpm())).strong().color(color).monospace());
    }

    fn track_panel(&mut self, ui: &mut Ui) {
        let Some(l) = &mut self.loaded else { return };
        let mut cmds = Vec::new();
        let mut select = None;
        ui.add_space(10.0);
        theme::section(ui, "Tracks");
        ui.add_space(4.0);
        let any_solo = l.mix.iter().any(|m| m.solo);
        self.track_rects.resize(l.song.tracks.len(), Rect::NOTHING);
        for (i, t) in l.song.tracks.iter().enumerate() {
            let m = &mut l.mix[i];
            let selected = self.view_track == i;
            // Registered before the contents so the buttons and sliders inside stay clickable.
            let bg = ui.interact(self.track_rects[i], ui.id().with(("track", i)), Sense::click());
            if bg.clicked() {
                select = Some(i);
            }
            let hint = if i < 9 { format!("Show this track (key {})", i + 1) } else { "Show this track".into() };
            let hovered = bg.on_hover_text(hint).hovered();
            let mut prepared = egui::Frame::new()
                .corner_radius(CornerRadius::same(10))
                .stroke(Stroke::new(1.0, if selected { BORDER } else { Color32::TRANSPARENT }))
                .inner_margin(Margin { left: 14, right: 8, top: 8, bottom: 8 })
                .begin(ui);
            {
                let ui = &mut prepared.content_ui;
                ui.set_width(ui.available_width());
                let audible = !m.mute && (!any_solo || m.solo);
                ui.horizontal(|ui| {
                    let name = if t.name.trim().is_empty() { t.instrument_name() } else { t.name.trim() };
                    let name = RichText::new(name).strong().color(if audible { TEXT } else { MUTED });
                    ui.add(egui::Label::new(name).selectable(false).truncate());
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(pill(m.solo, "S", WARN)).on_hover_text("Solo").clicked() {
                            m.solo = !m.solo;
                            cmds.push(Command::Solo(i, m.solo));
                        }
                        if ui.add(pill(m.mute, "M", DANGER)).on_hover_text("Mute").clicked() {
                            m.mute = !m.mute;
                            cmds.push(Command::Mute(i, m.mute));
                        }
                    });
                });
                let sub = if t.percussion { "Drums".to_string() } else { format!("{} · {}", t.instrument_name(), t.tuning_name()) };
                ui.add(egui::Label::new(RichText::new(sub).small().color(MUTED)).selectable(false).truncate());
                ui.spacing_mut().slider_width = (ui.available_width() - 4.0).max(40.0);
                if ui.add(egui::Slider::new(&mut m.volume, 0.0..=1.0).show_value(false)).on_hover_text("Track volume").changed() {
                    cmds.push(Command::Volume(i, m.volume));
                }
                if selected && !t.percussion {
                    let choices: &[ToneKind] = if m.tone.is_bass() { &ToneKind::ALL_BASS } else { &ToneKind::ALL_GUITAR };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Tone").small().color(MUTED));
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
            prepared.frame.fill = if selected {
                CARD
            } else if hovered {
                CARD_HOVER
            } else {
                Color32::TRANSPARENT
            };
            let resp = prepared.end(ui);
            self.track_rects[i] = resp.rect;
            let stripe = Rect::from_min_size(resp.rect.min + vec2(5.0, 10.0), vec2(3.0, resp.rect.height() - 20.0));
            ui.painter().rect_filled(stripe, 2.0, theme::TRACK_COLORS[i % theme::TRACK_COLORS.len()]);
            ui.add_space(2.0);
        }
        ui.add_space(14.0);
        egui::CollapsingHeader::new(RichText::new("Keyboard shortcuts").small().color(MUTED)).default_open(false).show(ui, |ui| {
            egui::Grid::new("keys").num_columns(2).spacing(vec2(12.0, 4.0)).show(ui, |ui| {
                for (k, d) in [
                    ("Space", "Play / pause"),
                    ("Click", "Move cursor"),
                    ("Drag", "Select & loop bars"),
                    ("L / Esc", "Loop / clear"),
                    ("← →", "Previous / next bar"),
                    ("Home", "Back to start"),
                    ("+ / −", "Tempo ±5 BPM"),
                    ("M", "Metronome"),
                    ("1–9", "Switch track"),
                    ("Ctrl+O", "Open file"),
                ] {
                    ui.label(RichText::new(k).small().monospace().color(TEXT));
                    ui.label(RichText::new(d).small().color(MUTED));
                    ui.end_row();
                }
            });
        });
        if let Some(i) = select {
            self.view_track = i;
        }
        for c in cmds {
            if let Command::Tone(_, k) = c {
                self.request_bank(k);
            }
            self.send(c);
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let w = 560.0f32.min(ui.available_width() - 40.0);
            ui.vertical_centered(|ui| {
                ui.add_space((ui.available_height() * 0.08).max(24.0));
                ui.add(egui::Image::new(&self.icon).fit_to_exact_size(vec2(96.0, 96.0)));
                ui.add_space(10.0);
                ui.label(RichText::new(APP_NAME).size(28.0).strong().color(TEXT));
                ui.label(RichText::new("Open a Guitar Pro tab, pick a track and play along at any tempo.").color(MUTED));
                ui.add_space(18.0);
                if ui.add(theme::primary_button("Open a tab…").min_size(vec2(180.0, 40.0))).clicked() {
                    self.open_dialog();
                }
                ui.add_space(4.0);
                ui.label(RichText::new("or drop a .gp3 / .gp4 / .gp5 file anywhere").small().color(MUTED));
                ui.add_space(22.0);

                let recent: Vec<PathBuf> = self.settings.recent.iter().filter(|p| p.is_file()).cloned().collect();
                if !recent.is_empty() {
                    ui.allocate_ui(vec2(w, 0.0), |ui| {
                        theme::card().show(ui, |ui| {
                            ui.set_width(w - 28.0);
                            theme::section(ui, "Recent");
                            ui.add_space(4.0);
                            for p in recent {
                                let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                                let dir = p.parent().map(|s| s.display().to_string()).unwrap_or_default();
                                let r = ui.add(
                                    egui::Button::new(RichText::new(name).color(TEXT))
                                        .right_text(RichText::new(dir).small().color(MUTED))
                                        .frame_when_inactive(false)
                                        .min_size(vec2(ui.available_width(), 30.0)),
                                );
                                if r.clicked() {
                                    self.open(&p);
                                }
                            }
                        });
                    });
                    ui.add_space(12.0);
                }
                ui.allocate_ui(vec2(w, 0.0), |ui| {
                    theme::card().show(ui, |ui| {
                        ui.set_width(w - 28.0);
                        self.setup_contents(ui, true);
                    });
                });
                ui.add_space(24.0);
            });
        });
    }

    /// Sound download + amp model rows (welcome screen and the Sounds dialog).
    fn setup_contents(&mut self, ui: &mut Ui, heading: bool) {
        if heading {
            theme::section(ui, "Sounds");
            ui.add_space(6.0);
        }
        ui.horizontal(|ui| {
            status_dot(ui, if self.sounds_ok { GOOD } else { WARN });
            ui.vertical(|ui| {
                ui.label(RichText::new("Bass & drum samples").strong().color(TEXT));
                let sub = if self.sounds_ok {
                    "Installed: picked and finger bass, drum kit."
                } else {
                    "Free bass and drum recordings (about 220 MB download)."
                };
                ui.label(RichText::new(sub).small().color(MUTED));
            });
            if !self.sounds_ok && self.download.is_none() {
                ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(theme::primary_button("Download")).clicked() {
                        self.start_download();
                    }
                });
            }
        });
        if let Some(p) = &self.download {
            let p = p.lock().unwrap().clone();
            ui.add_space(4.0);
            let text = if p.total > 0 {
                format!("{} · {:.0} / {:.0} MB", p.stage, p.done as f64 / 1e6, p.total as f64 / 1e6)
            } else {
                p.stage.clone()
            };
            if p.total > 1000 || p.total == 0 {
                ui.add(egui::ProgressBar::new(p.fraction()).text(text).animate(p.total == 0).fill(ACCENT));
            } else {
                ui.add(egui::ProgressBar::new(p.fraction()).text(format!("{} · {}/{}", p.stage, p.done, p.total)).fill(ACCENT));
            }
        }
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            let failed = matches!(*self.nam_state.lock().unwrap(), NamState::Failed(_));
            status_dot(ui, if self.nam_path.is_some() && !failed { GOOD } else { WARN });
            ui.vertical(|ui| {
                ui.label(RichText::new("Amp model (Neural Amp Modeler)").strong().color(TEXT));
                match &self.nam_path {
                    Some(p) => {
                        let name = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                        ui.label(RichText::new(name).small().color(MUTED));
                    }
                    None => {
                        ui.label(RichText::new("Pick a .nam capture for distorted guitar.").small().color(MUTED));
                    }
                }
                ui.hyperlink_to(RichText::new("Find free captures on TONE3000 ↗").small(), TONE3000_URL);
            });
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let label = if self.nam_path.is_some() { "Change…" } else { "Choose .nam…" };
                if ui.button(label).clicked() {
                    self.choose_nam();
                }
            });
        });
    }

    fn setup_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_setup {
            return;
        }
        let r = egui::Modal::new(egui::Id::new("setup"))
            .frame(egui::Frame::new().fill(SURFACE).corner_radius(14).inner_margin(20).stroke(Stroke::new(1.0, BORDER)))
            .show(ctx, |ui| {
                ui.set_width(520.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Sounds").size(20.0).strong().color(TEXT));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Done").clicked() {
                            self.show_setup = false;
                        }
                    });
                });
                ui.add_space(10.0);
                self.setup_contents(ui, false);
                if let Some(d) = setup::data_dir() {
                    ui.add_space(12.0);
                    ui.label(RichText::new(format!("Stored in {}", d.display())).small().color(MUTED));
                }
            });
        if r.should_close() {
            self.show_setup = false;
        }
    }

    fn tab_view(&mut self, ui: &mut Ui) {
        let Some(l) = &self.loaded else { return };
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
                    // Selecting bars turns on looping.
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
        let dropped: Vec<PathBuf> =
            ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if let Some(p) = dropped.first() {
            self.open(p);
        }
        self.poll_download();
        if !self.show_setup {
            self.handle_keys(ui);
        }

        let title = match &self.loaded {
            Some(l) if !l.song.title.is_empty() && !l.song.artist.is_empty() => format!("{} – {} · {APP_NAME}", l.song.artist, l.song.title),
            Some(l) if !l.song.title.is_empty() => format!("{} · {APP_NAME}", l.song.title),
            _ => APP_NAME.to_string(),
        };
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(16, 10)).stroke(Stroke::new(1.0, BORDER)))
            .show(ui, |ui| self.header(ui));
        if self.loaded.is_some() {
            egui::Panel::bottom("transport")
                .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin { left: 16, right: 16, top: 8, bottom: 10 }).stroke(Stroke::new(1.0, BORDER)))
                .show(ui, |ui| self.transport(ui));
            egui::Panel::left("tracks")
                .frame(egui::Frame::new().fill(SURFACE).inner_margin(Margin::symmetric(10, 4)))
                .default_size(270.0)
                .size_range(220.0..=420.0)
                .resizable(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| self.track_panel(ui));
                });
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(BG)).show(ui, |ui| {
            if self.loaded.is_some() {
                egui::Frame::new().inner_margin(Margin { left: 12, right: 12, top: 8, bottom: 0 }).show(ui, |ui| self.banners(ui));
                self.tab_view(ui);
            } else {
                egui::Frame::new().inner_margin(Margin::same(12)).show(ui, |ui| self.banners(ui));
                self.welcome(ui);
            }
        });
        self.setup_dialog(&ctx);

        if self.settings_dirty && !ctx.input(|i| i.pointer.any_down()) {
            self.settings.master = self.master;
            self.settings.metronome = self.metronome;
            self.settings.count_in = self.count_in;
            self.settings.save();
            self.settings_dirty = false;
        }
        if self.playing() {
            ctx.request_repaint();
        } else {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}

/// Round accent play/pause button.
fn play_button(ui: &mut Ui, playing: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(46.0, 46.0), if enabled { Sense::click() } else { Sense::hover() });
    let c = rect.center();
    let fill = if !enabled {
        CARD
    } else if resp.is_pointer_button_down_on() {
        Color32::from_rgb(230, 100, 55)
    } else if resp.hovered() {
        Color32::from_rgb(255, 145, 100)
    } else {
        ACCENT
    };
    let p = ui.painter();
    p.circle_filled(c, 23.0, fill);
    let ink = Color32::from_rgb(24, 16, 12);
    if playing {
        for dx in [-5.0, 5.0] {
            p.rect_filled(Rect::from_center_size(c + vec2(dx, 0.0), vec2(5.0, 18.0)), 1.5, ink);
        }
    } else {
        let pts = vec![c + vec2(-6.0, -10.0), c + vec2(11.0, 0.0), c + vec2(-6.0, 10.0)];
        p.add(egui::Shape::convex_polygon(pts, ink, Stroke::NONE));
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Small label/value pair in the transport bar.
fn stat(ui: &mut Ui, label: &str, value: &str) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(label).size(10.0).color(MUTED).strong());
        ui.label(RichText::new(value).monospace().color(TEXT));
    });
    ui.add_space(10.0);
}

/// Rounded on/off chip.
fn toggle_chip(on: bool, text: &str) -> egui::Button<'static> {
    let (fill, color, stroke) = if on {
        (theme::ACCENT_DIM, TEXT, Stroke::new(1.0, ACCENT))
    } else {
        (CARD, MUTED, Stroke::NONE)
    };
    egui::Button::new(RichText::new(text).color(color)).fill(fill).stroke(stroke).corner_radius(15.0).min_size(vec2(0.0, 30.0))
}

/// Compact M / S toggle.
fn pill(on: bool, text: &str, color: Color32) -> egui::Button<'static> {
    let (fill, ink) = if on { (color, Color32::from_rgb(20, 20, 24)) } else { (BG, MUTED) };
    egui::Button::new(RichText::new(text).small().strong().color(ink)).fill(fill).corner_radius(6.0).min_size(vec2(24.0, 22.0))
}

fn status_dot(ui: &mut Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}
