//! Attaching a real recording to a tab: the sidebar section, the alignment report and the
//! switch between hearing the synth and hearing the record.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui;
use egui::{Align, CornerRadius, Margin, RichText, Stroke, Ui, vec2};

use super::theme::{self, BORDER, CARD, DANGER, GOOD, MUTED, SURFACE, TEXT, WARN};
use super::{App, pill};
use crate::backing::{AUDIO_EXTENSIONS, AlignStatus, BackingSet, clock};
use crate::synth::{Command, Source};

/// Level controls for one stem, mirroring the track mixer.
pub struct StemMix {
    pub mute: bool,
    pub solo: bool,
    pub volume: f32,
}

/// A load running on a worker thread, reported back to the UI a line at a time.
#[derive(Default)]
pub struct Loading {
    pub status: Mutex<String>,
    pub done: Mutex<Option<Result<BackingSet, String>>>,
}

impl App {
    /// Ask for audio files and start lining them up.
    pub(super) fn add_audio_dialog(&mut self) {
        let dir = self.loaded.as_ref().and_then(|l| l.path.parent().map(|p| p.to_path_buf()));
        let mut d = rfd::FileDialog::new().add_filter("Audio", AUDIO_EXTENSIONS).add_filter("All files", &["*"]);
        if let Some(dir) = dir {
            d = d.set_directory(dir);
        }
        if let Some(paths) = d.pick_files() {
            self.load_audio(paths);
        }
    }

    /// Decode and align a set of files in the background.
    pub(super) fn load_audio(&mut self, paths: Vec<PathBuf>) {
        let Some(l) = &self.loaded else { return };
        if paths.is_empty() || self.backing_load.is_some() {
            return;
        }
        self.backing_error = None;
        let load = Arc::new(Loading::default());
        *load.status.lock().unwrap() = "Reading audio…".into();
        self.backing_load = Some(load.clone());
        let ctx = self.ctx.clone();
        let progress_ctx = ctx.clone();
        let progress_load = load.clone();
        crate::backing::load_async(
            paths,
            l.sched.clone(),
            move |msg| {
                *progress_load.status.lock().unwrap() = msg;
                progress_ctx.request_repaint();
            },
            move |result| {
                *load.done.lock().unwrap() = Some(result);
                ctx.request_repaint();
            },
        );
    }

    /// Line the loaded stems up again after their roles changed. The audio stays in memory;
    /// only the measuring is redone.
    pub(super) fn realign_audio(&mut self, kinds: Vec<crate::backing::StemKind>) {
        let Some(set) = self.backing.clone() else { return };
        let Some(l) = &self.loaded else { return };
        if self.backing_load.is_some() {
            return;
        }
        let sched = l.sched.clone();
        let load = Arc::new(Loading::default());
        *load.status.lock().unwrap() = "Lining the audio up again…".into();
        self.backing_load = Some(load.clone());
        let ctx = self.ctx.clone();
        std::thread::Builder::new()
            .name("backing-realign".into())
            .spawn(move || {
                let out = set.with_roles(&kinds, &sched);
                *load.done.lock().unwrap() = Some(Ok(out));
                ctx.request_repaint();
            })
            .ok();
    }

    /// Shift the recording against the tab by hand, in seconds. Positive plays it later.
    pub(super) fn set_trim(&mut self, secs: f64) {
        let t = secs.clamp(-5.0, 5.0);
        if t != self.backing_trim {
            self.backing_trim = t;
            self.send(Command::Trim(t));
            self.settings_dirty = true;
        }
    }

    /// Pick up a finished load. Called once a frame.
    pub(super) fn poll_audio(&mut self) {
        let Some(load) = &self.backing_load else {
            return;
        };
        let Some(result) = load.done.lock().unwrap().take() else {
            return;
        };
        self.backing_load = None;
        match result {
            Ok(set) => {
                let ok = set.align.ok();
                // Lining up again keeps whatever the mixer was set to, as the stems are the same.
                if self.stem_mix.len() != set.stems.len() {
                    self.stem_mix =
                        set.stems.iter().map(|_| StemMix { mute: false, solo: false, volume: 1.0 }).collect();
                }
                if let Some(l) = &self.loaded {
                    let path = l.path.clone();
                    self.settings.remember_audio(&path, &set.paths());
                    self.settings.remember_trim(&path, self.backing_trim);
                    self.settings.save();
                }
                let set = Arc::new(set);
                self.backing = Some(set.clone());
                self.send(Command::Backing(Some(set)));
                self.apply_stem_mix();
                // Nobody attaches a recording in order to keep listening to the synth.
                self.set_source(if ok { Source::Song } else { Source::Synth });
            }
            Err(e) => self.backing_error = Some(e),
        }
    }

    /// Reload the audio remembered for this tab, if any. Quietly does nothing otherwise.
    pub(super) fn restore_audio(&mut self) {
        let Some(l) = &self.loaded else { return };
        // Always taken from this tab, so a nudge made on the previous one doesn't carry over.
        let trim = self.settings.trim_for(&l.path);
        let paths = self.settings.audio_for(&l.path);
        self.set_trim(trim);
        if let Some(paths) = paths {
            self.load_audio(paths);
        }
    }

    pub(super) fn clear_audio(&mut self) {
        self.set_trim(0.0);
        self.backing = None;
        self.backing_error = None;
        self.stem_mix.clear();
        self.send(Command::Backing(None));
        self.set_source(Source::Synth);
        if let Some(l) = &self.loaded {
            let path = l.path.clone();
            self.settings.remember_audio(&path, &[]);
            self.settings.save();
        }
    }

    pub(super) fn set_source(&mut self, s: Source) {
        if self.source != s {
            self.source = s;
            self.send(Command::Source(s));
        }
    }

    /// True when there is a recording that actually lined up.
    pub(super) fn song_available(&self) -> bool {
        self.backing.as_ref().is_some_and(|b| b.align.ok())
    }

    /// Fold each stem's mute and the solo state into one gain and send it to the engine.
    pub(super) fn apply_stem_mix(&self) {
        let any_solo = self.stem_mix.iter().any(|m| m.solo);
        for (i, m) in self.stem_mix.iter().enumerate() {
            let on = !m.mute && (!any_solo || m.solo);
            self.send(Command::StemGain(i, if on { m.volume } else { 0.0 }));
        }
    }

    /// Sidebar section: attach audio, mix the stems, open the report.
    pub(super) fn song_panel(&mut self, ui: &mut Ui) {
        if self.loaded.is_none() {
            return;
        }
        ui.add_space(10.0);
        theme::section(ui, "Song audio");
        ui.add_space(4.0);

        if let Some(load) = &self.backing_load {
            let msg = load.status.lock().unwrap().clone();
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new(msg).small().color(MUTED));
            });
            ui.add_space(6.0);
            return;
        }

        if let Some(e) = self.backing_error.clone() {
            ui.label(RichText::new(e).small().color(DANGER));
            ui.add_space(4.0);
        }

        let Some(set) = self.backing.clone() else {
            ui.label(
                RichText::new("Play along with the record instead of the synth. Separated stems work best: add them all at once and they line up together.")
                    .small()
                    .color(MUTED),
            );
            ui.add_space(6.0);
            if ui
                .button("♫  Add audio…")
                .on_hover_text("Choose one mixdown, or every stem of the song at once")
                .clicked()
            {
                self.add_audio_dialog();
            }
            ui.add_space(6.0);
            return;
        };

        let a = &set.align;
        let (color, label) = match a.status {
            AlignStatus::Good => (GOOD, "Lined up"),
            AlignStatus::Warn => (WARN, "Lined up"),
            AlignStatus::Failed => (DANGER, "Not lined up"),
        };
        ui.horizontal(|ui| {
            ui.label(RichText::new("●").color(color));
            ui.label(RichText::new(label).small().strong().color(TEXT));
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("✖").on_hover_text("Remove this audio").clicked() {
                    self.clear_audio();
                }
                if ui.small_button("Details…").clicked() {
                    self.show_align = true;
                }
            });
        });
        ui.label(RichText::new(a.summary.clone()).small().color(MUTED));
        if self.backing.is_none() {
            return;
        }

        ui.add_space(6.0);
        let mut changed = false;
        let any_solo = self.stem_mix.iter().any(|m| m.solo);
        for (i, stem) in set.stems.iter().enumerate() {
            let Some(m) = self.stem_mix.get_mut(i) else {
                break;
            };
            let audible = !m.mute && (!any_solo || m.solo);
            egui::Frame::new()
                .fill(CARD)
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin { left: 10, right: 6, top: 6, bottom: 6 })
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let name = RichText::new(stem.kind.label()).strong().color(if audible { TEXT } else { MUTED });
                        ui.add(egui::Label::new(name).selectable(false).truncate());
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            if ui.add(pill(m.solo, "S", WARN)).on_hover_text("Solo").clicked() {
                                m.solo = !m.solo;
                                changed = true;
                            }
                            if ui
                                .add(pill(m.mute, "M", DANGER))
                                .on_hover_text("Mute this stem and play the part yourself")
                                .clicked()
                            {
                                m.mute = !m.mute;
                                changed = true;
                            }
                        });
                    });
                    ui.add(
                        egui::Label::new(RichText::new(&stem.name).small().color(MUTED)).selectable(false).truncate(),
                    );
                    ui.spacing_mut().slider_width = (ui.available_width() - 4.0).max(40.0);
                    if ui.add(egui::Slider::new(&mut m.volume, 0.0..=1.5).show_value(false)).changed() {
                        changed = true;
                    }
                });
            ui.add_space(3.0);
        }
        if changed {
            self.apply_stem_mix();
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Level").small().color(MUTED));
            ui.spacing_mut().slider_width = (ui.available_width() - 8.0).max(40.0);
            if ui
                .add(egui::Slider::new(&mut self.backing_volume, 0.0..=1.5).show_value(false))
                .on_hover_text("Volume of the recording as a whole")
                .changed()
            {
                self.send(Command::BackingVolume(self.backing_volume));
                self.settings_dirty = true;
            }
        });
        ui.add_space(6.0);
    }

    /// The full alignment report, including every check through the song.
    pub(super) fn align_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_align {
            return;
        }
        let Some(set) = self.backing.clone() else {
            self.show_align = false;
            return;
        };
        let a = &set.align;
        // Collected inside the modal and acted on after it, so the borrow of `set` is done with.
        let mut roles: Option<Vec<crate::backing::StemKind>> = None;
        let mut trim: Option<f64> = None;
        let r = egui::Modal::new(egui::Id::new("align"))
            .frame(egui::Frame::new().fill(SURFACE).corner_radius(14).inner_margin(20).stroke(Stroke::new(1.0, BORDER)))
            .show(ctx, |ui| {
                ui.set_width(620.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("How the audio lines up").size(20.0).strong().color(TEXT));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Done").clicked() {
                            self.show_align = false;
                        }
                    });
                });
                ui.add_space(10.0);

                let color = match a.status {
                    AlignStatus::Good => GOOD,
                    AlignStatus::Warn => WARN,
                    AlignStatus::Failed => DANGER,
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new("●").color(color));
                    ui.label(RichText::new(&a.summary).strong().color(TEXT));
                });
                ui.add_space(8.0);

                // The recording is the clock: say where the band comes in and how the tab has to
                // be stretched to keep up with it, section by section.
                let (lo, hi) = a.tempo_range();
                let word = |p: f64| {
                    if p.abs() < 0.05 {
                        "the tab's tempo".to_string()
                    } else if p > 0.0 {
                        format!("{p:.1}% faster than the tab")
                    } else {
                        format!("{:.1}% slower than the tab", -p)
                    }
                };
                let pace = if (hi - lo).abs() < 1.0 {
                    format!("plays at {}", word(a.tempo_pct()))
                } else {
                    format!("plays between {} and {}", word(lo), word(hi))
                };
                ui.label(
                    RichText::new(format!("The band comes in {:.2}s into the recording and {pace}.", a.offset))
                        .size(15.0)
                        .color(TEXT),
                );
                ui.label(
                    RichText::new(
                        "In Song mode the recording keeps time and the tab follows it; the recording is \
                         never sped up or slowed down to fit the tab.",
                    )
                    .small()
                    .color(MUTED),
                );
                ui.add_space(4.0);

                // Nothing here assumes the recording starts or ends with the tab, and this says so
                // out loud: the lead-in is whatever count-in or extra bars the recording happens to
                // have, and the tail is whatever runs on after the tab is finished.
                if let Some(audio_end) = set.stems.iter().map(|s| s.duration()).reduce(f64::max) {
                    let score_end = self.loaded.as_ref().map(|l| l.sched.timeline.duration()).unwrap_or(0.0);
                    let tail = audio_end - a.audio_time(score_end);
                    ui.label(
                        RichText::new(format!(
                            "{:.1}s of the recording runs before the tab starts and {:.1}s after it ends; \
                             neither end is assumed to match.",
                            a.offset.max(0.0),
                            tail.max(0.0)
                        ))
                        .small()
                        .color(MUTED),
                    );
                }

                // The honest measure of whether this worked: how far the tab's hits land from the
                // record's. Milliseconds can be judged by ear; a correlation cannot.
                if a.hit_error.is_finite() {
                    let ms = a.hit_error * 1000.0;
                    let (word, color) = match ms {
                        m if m <= 40.0 => ("tight enough to sound like one performance", GOOD),
                        m if m <= 80.0 => ("close enough to play along to", GOOD),
                        m if m <= 150.0 => ("loose; some parts may feel flammed", WARN),
                        _ => ("too loose to play along to", DANGER),
                    };
                    let part = if a.hit_part.is_empty() {
                        String::new()
                    } else {
                        format!(" (measured on {})", a.hit_part.to_lowercase())
                    };
                    ui.label(
                        RichText::new(format!("The tab's hits land within {ms:.0}ms of the record's{part} - {word}."))
                            .small()
                            .color(color),
                    )
                    .on_hover_text(
                        "Measured on the part with the sharpest attack, which is the only kind that can pin down \
                         a beat. Bass, chords and voices swell in rather than strike, so they measure worse \
                         without being lined up any differently - every part shares one alignment.",
                    );
                }
                ui.add_space(6.0);

                // The hand nudge: the one control to reach for if it still sounds early or late.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Nudge by hand").small().strong().color(TEXT));
                    for (text, by) in [("−100ms", -0.1), ("−20ms", -0.02), ("+20ms", 0.02), ("+100ms", 0.1)] {
                        if ui.small_button(text).clicked() {
                            trim = Some(self.backing_trim + by);
                        }
                    }
                    if self.backing_trim != 0.0 {
                        ui.label(RichText::new(format!("{:+.0}ms", self.backing_trim * 1000.0)).small().color(WARN));
                        if ui.small_button("Reset").clicked() {
                            trim = Some(0.0);
                        }
                    } else {
                        ui.label(RichText::new("none").small().color(MUTED));
                    }
                });
                ui.add_space(8.0);
                ui.label(RichText::new(&a.detail).color(MUTED));

                ui.add_space(10.0);
                ui.label(RichText::new("Stems").small().strong().color(TEXT));
                ui.label(
                    RichText::new(
                        "What a stem holds decides the part of the tab it is matched against. Change it if the \
                         guess is wrong and everything is measured again.",
                    )
                    .small()
                    .color(MUTED),
                );
                ui.add_space(4.0);
                egui::Grid::new("stems").num_columns(4).spacing(vec2(14.0, 4.0)).show(ui, |ui| {
                    for (i, stem) in set.stems.iter().enumerate() {
                        let mut kind = stem.kind;
                        egui::ComboBox::from_id_salt(("stemkind", i))
                            .selected_text(RichText::new(kind.label()).small())
                            .width(90.0)
                            .show_ui(ui, |ui| {
                                for k in crate::backing::StemKind::ALL {
                                    ui.selectable_value(&mut kind, k, k.label()).on_hover_text(k.hint());
                                }
                            });
                        if kind != stem.kind {
                            let mut kinds: Vec<_> = set.stems.iter().map(|s| s.kind).collect();
                            kinds[i] = kind;
                            roles = Some(kinds);
                        }
                        ui.label(RichText::new(&stem.name).small().color(MUTED));
                        ui.label(
                            RichText::new(format!("{} · {:.0} kHz", clock(stem.duration()), stem.sr as f64 / 1000.0))
                                .small()
                                .color(MUTED),
                        );
                        let (verdict, strong) = stem.agreement();
                        ui.label(RichText::new(verdict).small().color(if strong { GOOD } else { MUTED })).on_hover_text(
                            "How far this part's hits in the tab land from the nearest hit in the recording, once \
                             the alignment is applied. Under about 40ms is tight enough that the two sound like one \
                             performance; a real band plays a little loose, so some spread is the band, not an error.",
                        );
                        ui.end_row();
                    }
                });
                ui.label(
                    RichText::new(format!("Held in memory: {} MB", set.memory_bytes() / 1_000_000))
                        .small()
                        .color(MUTED),
                );

                ui.add_space(10.0);
                // Folded away by default: it is long, and the summary above already says how it went.
                egui::CollapsingHeader::new(
                    RichText::new(format!("Checks through the song ({})", a.checks.len())).small().strong().color(TEXT),
                )
                .id_salt("checks-fold")
                .show_unindented(ui, |ui| {
                    ui.allocate_ui(vec2(ui.available_width(), 220.0), |ui| {
                        egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                            egui::Grid::new("checks").num_columns(4).spacing(vec2(16.0, 2.0)).striped(true).show(ui, |ui| {
                                for h in ["Time", "Bar", "Nudge", "Match"] {
                                    ui.label(RichText::new(h).small().strong().color(MUTED));
                                }
                                ui.end_row();
                                for c in &a.checks {
                                    ui.label(RichText::new(clock(c.time)).small().monospace().color(MUTED));
                                    ui.label(RichText::new(format!("{}", c.bar)).small().monospace().color(MUTED));
                                    if c.matched {
                                        ui.label(RichText::new(format!("{:+.2}s", c.drift)).small().monospace().color(TEXT));
                                        ui.label(RichText::new(format!("{:.0}%", c.confidence * 100.0)).small().monospace().color(MUTED));
                                    } else {
                                        ui.label(RichText::new("—").small().monospace().color(MUTED));
                                        ui.label(RichText::new("nothing to match").small().color(MUTED));
                                    }
                                    ui.end_row();
                                }
                            });
                        });
                    });
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Nudge is how far lining up the hits there moved the tab from where the song's structure \
                             put it. Bar numbers repeat where the song does.",
                        )
                        .small()
                        .color(MUTED),
                    );
                });
            });
        if r.should_close() {
            self.show_align = false;
        }
        if let Some(t) = trim {
            self.set_trim(t);
        }
        if let Some(kinds) = roles {
            drop(set);
            self.realign_audio(kinds);
        }
    }

    /// Synth / Song switch for the transport bar.
    pub(super) fn source_switch(&mut self, ui: &mut Ui) {
        if self.backing.is_none() {
            return;
        }
        let ok = self.song_available();
        let song = self.source == Source::Song;
        if ui.add(super::toggle_chip(!song, "🎛 Synth")).on_hover_text("Hear the tab played by the synth (S)").clicked()
        {
            self.set_source(Source::Synth);
        }
        let hint = if ok {
            "Hear the recording; the tab follows it (S)"
        } else {
            "The recording could not be lined up with this tab"
        };
        if ui.add_enabled(ok, super::toggle_chip(song, "🎵 Song")).on_hover_text(hint).clicked() {
            self.set_source(Source::Song);
        }
    }
}
