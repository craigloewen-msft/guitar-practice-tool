#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod audio;
mod backing;
mod gp;
mod model;
mod setup;
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
            let mut p: Vec<f32> = evs.iter().map(|e| e.pitch).collect();
            p.sort_by(f32::total_cmp);
            let q = |f: f32| p[((p.len() - 1) as f32 * f) as usize];
            let pm = evs.iter().filter(|e| e.palm_mute).count();
            println!(
                "track {t}: {} events ({pm} palm-muted), first in measure {ma}, last in measure {mb}, pitch min/p10/median/p90/max {}/{}/{}/{}/{}",
                evs.len(),
                q(0.0),
                q(0.1),
                q(0.5),
                q(0.9),
                q(1.0)
            );
        }
    }
    Ok(())
}

fn arg_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(|s| s.as_str())
}

/// Offline render: `--render <song> <out.wav> [--speed 0.5] [--solo N] [--start-measure M] [--seconds S]
/// [--synth] [--nam model.nam | --no-nam] [--single]`
fn render(args: &[String]) -> anyhow::Result<()> {
    use std::sync::Arc;
    use synth::{Command, Engine, Shared};
    let song = gp::load_file(std::path::Path::new(&args[1]))?;
    let out = PathBuf::from(&args[2]);
    let speed: f64 = arg_value(args, "--speed").map(|s| s.parse()).transpose()?.unwrap_or(1.0);
    let seconds: f64 = arg_value(args, "--seconds").map(|s| s.parse()).transpose()?.unwrap_or(20.0);
    let start_measure: usize = arg_value(args, "--start-measure").map(|s| s.parse()).transpose()?.unwrap_or(1);
    let sr = 48000u32;
    let nam_path = if args.iter().any(|a| a == "--no-nam") {
        None
    } else {
        arg_value(args, "--nam").map(PathBuf::from).or_else(synth::nam::find_model)
    };
    let tones: Vec<_> = song.tracks.iter().map(|t| t.default_tone()).collect();
    let sched = Arc::new(timeline::build_schedule(&song, &tones));
    let start_tick = sched
        .timeline
        .occurrence(start_measure.saturating_sub(1), 0)
        .map(|m| m.start)
        .unwrap_or(0);
    let mut engine = Engine::new(sr as f32, Arc::new(Shared::default()), None);
    engine.handle(Command::SetSchedule(sched.clone()));
    engine.handle(Command::NamOptions(synth::NamOptions {
        double_track: !args.iter().any(|a| a == "--single"),
    }));
    if tones.iter().any(|t| matches!(t, model::ToneKind::Nam | model::ToneKind::Clean)) {
        match &nam_path {
            Some(p) => {
                let m = synth::nam::NamModel::load(p)?;
                println!("NAM model: {} ({})", m.name, p.display());
                engine.handle(Command::Nam(Some(Arc::new(m))));
            }
            None => println!("no .nam model found; guitars use a basic built-in amp"),
        }
    }
    if !args.iter().any(|a| a == "--synth") {
        let lib = synth::sampler::Library::new();
        let mut ids: Vec<_> = tones.iter().filter_map(|&t| synth::sampler::BankId::for_tone(t)).collect();
        ids.sort_by_key(|i| i.index());
        ids.dedup();
        for id in ids {
            let t = std::time::Instant::now();
            match lib.load_now(id) {
                Some(b) => {
                    println!("loaded {} in {:.2}s ({} MB)", id.label(), t.elapsed().as_secs_f64(), b.memory_bytes() >> 20);
                    engine.handle(Command::Bank(id, b));
                }
                None => println!("bank {} not available, using synth", id.label()),
            }
        }
    }
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

/// Run a mono WAV through a NAM capture: `--nam-render <model.nam> <in.wav> <out.wav>`
/// (writes 32-bit float, like NeuralAmpModelerCore's `render` tool).
fn nam_render(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(args.len() >= 4, "usage: --nam-render <model.nam> <in.wav> <out.wav>");
    let mut model = synth::nam::NamModel::load(std::path::Path::new(&args[1]))?;
    let mut reader = hound::WavReader::open(&args[2])?;
    let spec = reader.spec();
    let input: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.samples::<f32>().step_by(spec.channels as usize).collect::<Result<_, _>>()?,
        hound::SampleFormat::Int => {
            let scale = 1.0 / (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .samples::<i32>()
                .step_by(spec.channels as usize)
                .map(|s| s.map(|v| v as f32 * scale))
                .collect::<Result<_, _>>()?
        }
    };
    if spec.sample_rate as f32 != model.sample_rate {
        eprintln!("warning: input is {} Hz but the model expects {} Hz", spec.sample_rate, model.sample_rate);
    }
    let mut out = vec![0.0; input.len()];
    let t0 = std::time::Instant::now();
    model.process(&input, &mut out);
    let el = t0.elapsed().as_secs_f64();
    println!(
        "{}: {} samples in {el:.2}s ({:.1}x realtime)",
        model.name,
        input.len(),
        input.len() as f64 / model.sample_rate as f64 / el
    );
    let spec_out = hound::WavSpec {
        channels: 1,
        sample_rate: model.sample_rate as u32,
        bits_per_sample: 32,
        sample_format: hound::SampleFormat::Float,
    };
    let mut w = hound::WavWriter::create(&args[3], spec_out)?;
    for s in out {
        w.write_sample(s)?;
    }
    w.finalize()?;
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(|s| s.as_str()) {
        Some("--dump") => dump(&PathBuf::from(&args[1])),
        Some("--render") => render(&args),
        Some("--nam-render") => nam_render(&args),
        Some("--download-sounds") => download_sounds(),
        Some("--align") => align(&args),
        Some("--help") | Some("-h") => {
            println!("usage: guitar-practice-tool [song.gp5]\n       guitar-practice-tool --dump <song>\n       guitar-practice-tool --render <song> <out.wav> [--speed x] [--solo n] [--start-measure m] [--seconds s]\n           [--synth] [--nam model.nam | --no-nam] [--single]\n       guitar-practice-tool --nam-render <model.nam> <in.wav> <out.wav>\n       guitar-practice-tool --align <song> <audio...>\n       guitar-practice-tool --download-sounds");
            Ok(())
        }
        _ => run_gui(args.first().map(PathBuf::from)),
    }
}

/// Report how a recording (or a set of stems) lines up with a tab, without opening the GUI.
fn align(args: &[String]) -> anyhow::Result<()> {
    if args.len() < 3 {
        anyhow::bail!("usage: --align <song.gp5> <audio...>");
    }
    let song = gp::load_file(&PathBuf::from(&args[1]))?;
    let tones: Vec<_> = song.tracks.iter().map(|t| t.default_tone()).collect();
    let sched = timeline::build_schedule(&song, &tones);
    let paths: Vec<PathBuf> = args[2..].iter().map(PathBuf::from).collect();
    let t0 = std::time::Instant::now();
    let b = backing::BackingSet::load(&paths, &sched, |s| println!("{s}")).map_err(|e| anyhow::anyhow!(e))?;
    println!("analysed in {:.1}s", t0.elapsed().as_secs_f64());
    for s in &b.stems {
        println!(
            "stem {:<10} hits within {:>4.0}ms ({:>3.0}% found), corr {:>4.0}%  {}",
            s.kind.label(),
            s.error * 1000.0,
            s.found * 100.0,
            s.confidence * 100.0,
            s.name
        );
    }
    println!("{}", b.align.detail);
    println!("status: {:?} - {}", b.align.status, b.align.summary);
    println!("{:>8}  {:>6}  {:>9}  {:>6}", "time", "bar", "drift", "match");
    for c in &b.align.checks {
        println!(
            "{:>7.1}s  {:>6}  {:>8.3}s  {:>5.0}%{}",
            c.time,
            c.bar,
            c.drift,
            c.confidence * 100.0,
            if c.matched { "" } else { "  (not confirmed)" }
        );
    }
    // Where each bar of the tab lands in the recording, in playback order, so a report like
    // "at bar 13 the record is playing bar 17" can be checked against the numbers directly.
    if std::env::var_os("GPT_ALIGN_BARS").is_some() {
        let tl = &sched.timeline;
        println!("{:>5}  {:>4}  {:>6}  {:>9}  {:>9}", "play", "bar", "bpm", "tab", "record");
        for (k, m) in tl.measures.iter().enumerate() {
            let s = tl.seconds_at(m.start as f64);
            println!(
                "{:>5}  {:>4}  {:>6.1}  {:>8.2}s  {:>8.2}s",
                k + 1,
                m.index + 1,
                tl.bpm_at(m.start),
                s,
                b.align.audio_time(s)
            );
        }
    }
    Ok(())
}

fn download_sounds() -> anyhow::Result<()> {
    let lib = synth::sampler::Library::new();
    if setup::sounds_installed(&lib) {
        println!("All sounds are already installed.");
        return Ok(());
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let progress = setup::start_download(lib, move || {
        let _ = tx.send(());
    });
    let mut last = String::new();
    while rx.recv_timeout(std::time::Duration::from_millis(500)).is_err() {
        let p = progress.lock().unwrap().clone();
        let line = format!("{} {:>3.0}%", p.stage, p.fraction() * 100.0);
        if line != last {
            println!("{line}");
            last = line;
        }
    }
    let r = progress.lock().unwrap().finished.clone().unwrap_or(Ok(()));
    r.map_err(|e| anyhow::anyhow!(e))?;
    println!("Sounds installed in {}", setup::data_dir().unwrap().display());
    Ok(())
}

fn run_gui(path: Option<PathBuf>) -> anyhow::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(ui::APP_NAME)
            .with_app_id("guitar-practice-tool")
            .with_icon(ui::icon())
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([820.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(ui::APP_NAME, options, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, path)))))
        .map_err(|e| anyhow::anyhow!("{e}"))
}
