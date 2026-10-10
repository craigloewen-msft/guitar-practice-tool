mod audio;
mod gp;
mod model;
mod synth;
mod timeline;
mod ui;

use eframe::egui;

use std::path::PathBuf;

fn dump(path: &PathBuf) -> anyhow::Result<()> {
    let song = gp::load_file(path)?;
    println!("title={:?} artist={:?} tempo={} measures={}", song.title, song.artist, song.tempo, song.measure_headers.len());
    for (i, t) in song.tracks.iter().enumerate() {
        println!(
            "track {i}: {:?} program={} perc={} strings={:?} vol={} bal={} measures={} tone={:?}",
            t.name, t.program, t.percussion, t.strings, t.volume, t.balance, t.measures.len(), t.default_tone()
        );
    }
    for h in &song.measure_headers {
        if h.repeat_open || h.repeat_close > 0 || h.repeat_alternative != 0 {
            println!("m{} open={} close={} alt={}", h.number, h.repeat_open, h.repeat_close, h.repeat_alternative);
        }
    }
    let mut c = std::collections::BTreeMap::<&str, usize>::new();
    for t in &song.tracks {
        for m in &t.measures {
            for v in &m.voices {
                for b in &v.beats {
                    if b.tempo_change.is_some() { *c.entry("tempo").or_default() += 1; }
                    if b.effects.wide_vibrato { *c.entry("beat_vibrato").or_default() += 1; }
                    if b.effects.tremolo_bar.is_some() { *c.entry("tremolo_bar").or_default() += 1; }
                    if b.effects.stroke.is_some() { *c.entry("stroke").or_default() += 1; }
                    for n in &b.notes {
                        let e = &n.effects;
                        if e.palm_mute { *c.entry("palm_mute").or_default() += 1; }
                        if n.kind == model::NoteKind::Tie { *c.entry("tie").or_default() += 1; }
                        if n.kind == model::NoteKind::Dead { *c.entry("dead").or_default() += 1; }
                        if e.hammer { *c.entry("hammer").or_default() += 1; }
                        if e.let_ring { *c.entry("let_ring").or_default() += 1; }
                        if e.has_slide(model::Slide::LegatoTo) { *c.entry("legato_slide").or_default() += 1; }
                        if e.bend.is_some() { *c.entry("bend").or_default() += 1; }
                        if e.vibrato { *c.entry("vibrato").or_default() += 1; }
                        if e.harmonic.is_some() { *c.entry("harmonic").or_default() += 1; }
                        if e.grace.is_some() { *c.entry("grace").or_default() += 1; }
                    }
                }
            }
        }
    }
    println!("{c:?}");
    let tl = timeline::Timeline::build(&song);
    println!(
        "playback measures={} total_ticks={} duration={:.1}s tempo_changes={}",
        tl.measures.len(),
        tl.total,
        tl.seconds_at(tl.total as f64),
        tl.tempo_map.len()
    );
    let tones: Vec<_> = song.tracks.iter().map(|t| t.default_tone()).collect();
    let sched = timeline::build_schedule(&song, &tones);
    println!("events={} clicks={}", sched.events.len(), sched.clicks.len());
    for t in 0..song.tracks.len() {
        let evs: Vec<_> = sched.events.iter().filter(|e| e.track == t).collect();
        if let (Some(a), Some(b)) = (evs.first(), evs.last()) {
            let ma = sched.timeline.measures[sched.timeline.locate(a.start).unwrap()].index + 1;
            let mb = sched.timeline.measures[sched.timeline.locate(b.start).unwrap()].index + 1;
            println!("track {t}: {} events, first in measure {ma}, last in measure {mb}", evs.len());
        }
    }
    Ok(())
}

fn arg_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

/// Offline render: `--render <song> <out.wav> [--speed 0.5] [--solo N] [--start-measure M] [--seconds S]`
fn render(args: &[String]) -> anyhow::Result<()> {
    use std::sync::Arc;
    use synth::{Command, Engine, Shared};
    let song = gp::load_file(std::path::Path::new(&args[1]))?;
    let out = PathBuf::from(&args[2]);
    let speed: f64 = arg_value(args, "--speed").map(|s| s.parse()).transpose()?.unwrap_or(1.0);
    let seconds: f64 = arg_value(args, "--seconds").map(|s| s.parse()).transpose()?.unwrap_or(20.0);
    let start_measure: usize = arg_value(args, "--start-measure").map(|s| s.parse()).transpose()?.unwrap_or(1);
    let sr = 48000u32;
    let tones: Vec<_> = song.tracks.iter().map(|t| t.default_tone()).collect();
    let sched = Arc::new(timeline::build_schedule(&song, &tones));
    let start_tick = sched
        .timeline
        .occurrence(start_measure.saturating_sub(1), 0)
        .map(|m| m.start)
        .unwrap_or(0);
    let mut engine = Engine::new(sr as f32, Arc::new(Shared::default()), None);
    engine.handle(Command::SetSchedule(sched.clone()));
    if let Some(s) = arg_value(args, "--solo") {
        engine.handle(Command::Solo(s.parse()?, true));
    }
    engine.handle(Command::Speed(speed));
    engine.handle(Command::Seek(start_tick));
    engine.handle(Command::Play);
    let frames = (seconds * sr as f64) as usize;
    let mut buf = vec![0.0f32; frames * 2];
    let t0 = std::time::Instant::now();
    for chunk in buf.chunks_mut(512 * 2) {
        engine.process(chunk, 2);
    }
    let el = t0.elapsed().as_secs_f64();
    let peak = buf.iter().fold(0.0f32, |a, &b| a.max(b.abs()));
    let rms = (buf.iter().map(|x| (x * x) as f64).sum::<f64>() / buf.len() as f64).sqrt();
    let nan = buf.iter().any(|x| !x.is_finite());
    println!(
        "rendered {seconds}s in {el:.2}s ({:.0}x realtime) peak={peak:.3} rms={rms:.4} ({:.1} dBFS) nan={nan}",
        seconds / el,
        20.0 * rms.log10()
    );
    audio::write_wav(&out, sr, &buf)?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("--dump") => dump(&PathBuf::from(&args[1])),
        Some("--render") => render(&args),
        Some("--help") | Some("-h") => {
            println!("usage: guitar-practice-tool [song.gp5]\n       guitar-practice-tool --dump <song>\n       guitar-practice-tool --render <song> <out.wav> [--speed x] [--solo n] [--start-measure m] [--seconds s]");
            Ok(())
        }
        _ => run_gui(args.first().map(PathBuf::from)),
    }
}

fn run_gui(path: Option<PathBuf>) -> anyhow::Result<()> {
    // With no argument, pick up a Guitar Pro file in the current directory if there is one.
    let path = path.or_else(|| {
        let mut found: Vec<PathBuf> = std::fs::read_dir(".")
            .ok()?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "gp3" | "gp4" | "gp5"))
            })
            .collect();
        found.sort();
        found.into_iter().next()
    });
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Guitar Practice")
            .with_app_id("guitar-practice-tool")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([700.0, 400.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "Guitar Practice",
        options,
        Box::new(move |cc| Ok(Box::new(ui::App::new(cc, path)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
