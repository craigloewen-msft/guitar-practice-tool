//! Offscreen UI renders for visual checks:
//! `GPT_SHOT_SONG=song.gp5 GPT_SHOTS=/tmp/shots cargo test --release shots -- --ignored`.

use std::path::PathBuf;

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

use super::App;

fn save(h: &mut Harness<'_, App>, name: &str) {
    let dir = PathBuf::from(std::env::var("GPT_SHOTS").unwrap_or_else(|_| "/tmp/gpt-shots".into()));
    std::fs::create_dir_all(&dir).unwrap();
    h.render().expect("render").save(dir.join(format!("{name}.png"))).unwrap();
}

fn harness(path: Option<PathBuf>) -> Harness<'static, App> {
    Harness::builder()
        .with_size([1400.0, 860.0])
        .with_max_steps(20)
        .wgpu()
        .build_eframe(move |cc| App::new(cc, path))
}

pub(crate) fn test_song() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GPT_SHOT_SONG") {
        return Some(p.into());
    }
    std::fs::read_dir(env!("CARGO_MANIFEST_DIR")).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.extension().is_some_and(|x| ["gp3", "gp4", "gp5"].iter().any(|g| x.eq_ignore_ascii_case(g)))
    })
}

#[test]
#[ignore]
fn shots() {
    let mut h = harness(None);
    h.run_steps(5);
    save(&mut h, "welcome");

    h.get_by_label("⚙  Sounds").click();
    h.run_steps(3);
    save(&mut h, "setup-modal");
    h.get_by_label("Done").click();
    h.run_steps(3);

    let Some(song) = test_song() else {
        eprintln!("skipping song shots: set GPT_SHOT_SONG or put a .gp5 in the repo root");
        return;
    };
    let mut h = harness(Some(song));
    std::thread::sleep(std::time::Duration::from_secs(4));
    h.run_steps(5);
    let w = h.state().track_rects[0].width();
    h.run_steps(5);
    assert_eq!(w, h.state().track_rects[0].width(), "track list must not grow frame to frame");
    save(&mut h, "loaded");

    // Mute + solo inside a track card, then select another card.
    h.get_all_by_label("M").next().unwrap().click();
    h.run_steps(2);
    h.get_all_by_label("S").nth(1).unwrap().click();
    h.run_steps(2);
    assert!(h.state().loaded.as_ref().unwrap().mix[0].mute, "mute click did not register");
    assert!(h.state().loaded.as_ref().unwrap().mix[1].solo, "solo click did not register");
    assert_eq!(h.state().view_track, 0, "M/S clicks must not select the card");
    h.get_all_by_label("Drums").next().unwrap().click();
    h.run_steps(3);
    save(&mut h, "tracks");
    assert_ne!(h.state().view_track, 0, "card click did not select the track");

    h.key_press(eframe::egui::Key::Plus);
    h.key_press(eframe::egui::Key::Plus);
    h.run_steps(3);
    save(&mut h, "tempo");
    let orig = h.state().orig_bpm();
    h.state_mut().set_bpm(orig);
    h.run_steps(2);

    let stems = test_audio();
    if stems.is_empty() {
        eprintln!("skipping song-audio shots: put audio files in the repo root");
        return;
    }
    // The tab may already have audio remembered from a previous run.
    if h.state().backing.is_none() && h.state().backing_load.is_none() {
        h.state_mut().load_audio(stems);
    }
    for _ in 0..200 {
        h.run_steps(1);
        if h.state().backing.is_some() || h.state().backing_error.is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    h.run_steps(3);
    assert!(h.state().backing_error.is_none(), "load failed: {:?}", h.state().backing_error);
    let set = h.state().backing.clone().expect("audio never finished loading");
    assert_eq!(h.state().stem_mix.len(), set.stems.len(), "one mixer row per stem");
    save(&mut h, "song-audio");

    assert!(set.align.ok(), "test audio did not line up: {}", set.align.summary);
    assert_eq!(h.state().source, crate::synth::Source::Song, "loading audio should switch to the record");

    h.get_by_label("Details…").click();
    h.run_steps(8);
    save(&mut h, "song-align");
    h.get_by_label("Done").click();
    h.run_steps(2);

    // Mute the guitar stem: the point of the whole feature.
    let guitar = set
        .stems
        .iter()
        .position(|s| matches!(s.kind, crate::backing::StemKind::Guitar | crate::backing::StemKind::Other));
    if let Some(i) = guitar {
        h.state_mut().stem_mix[i].mute = true;
        h.state().apply_stem_mix();
        h.run_steps(2);
    }

    h.get_by_label("🎛 Synth").click();
    h.run_steps(2);
    assert_eq!(h.state().source, crate::synth::Source::Synth, "Synth chip did not switch back");
    h.get_by_label("🎵 Song").click();
    h.run_steps(2);
    assert_eq!(h.state().source, crate::synth::Source::Song, "Song chip did not switch");

    // Attached audio is remembered and comes back with the tab.
    let song = test_song().unwrap();
    assert_eq!(
        h.state().settings.audio_for(&song).map(|v| v.len()),
        Some(set.stems.len()),
        "stem paths were not remembered for this tab"
    );

    h.get_by_label("✖").click();
    h.run_steps(3);
    assert!(h.state().backing.is_none(), "remove did not clear the audio");
    assert_eq!(h.state().source, crate::synth::Source::Synth, "removing audio must fall back to the synth");
    assert!(h.state().settings.audio_for(&song).is_none(), "remove must forget the stems");
}

/// Audio files sitting next to the test tab, for the song-audio shots.
fn test_audio() -> Vec<PathBuf> {
    let Ok(dir) = std::fs::read_dir(env!("CARGO_MANIFEST_DIR")) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| crate::backing::AUDIO_EXTENSIONS.iter().any(|a| x.eq_ignore_ascii_case(a)))
        })
        .collect();
    v.sort();
    // A set of stems beats a lone mixdown; drop any full mix if there are stems.
    if v.iter().any(|p| crate::backing::StemKind::from_name(&p.to_string_lossy()) != crate::backing::StemKind::Mix) {
        v.retain(|p| crate::backing::StemKind::from_name(&p.to_string_lossy()) != crate::backing::StemKind::Mix);
    }
    v
}
