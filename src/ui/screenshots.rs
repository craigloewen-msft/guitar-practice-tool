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
}
