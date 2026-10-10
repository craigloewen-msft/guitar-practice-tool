# Guitar Practice Tool

An offline, Songsterr-style tab player written in Rust. It loads Guitar Pro files
(`.gp3`, `.gp4`, `.gp5`), renders the tab, and plays it back with a built-in synth.

## Build & run

```sh
cargo build --release
./target/release/guitar-practice-tool song.gp5   # or no argument: opens the first .gp3/4/5 in the cwd
```

You can also open files from the **Open…** button, with Ctrl+O, or by dragging a file onto the window.

## Features

- Tab view with rhythm notation, bends, slides, hammer-ons/pull-offs, vibrato,
  palm-mute/let-ring runs, ghost/tied notes, harmonics, repeats and tempo changes.
- Track list (guitars, bass, drums) with per-track mute/solo, volume and tone preset.
  The view shows the selected track, but every track plays together.
- Synth: Karplus–Strong plucked strings with amp/cab simulation (clean, crunch,
  distortion), synthesized drum kit and reverb. It handles bends, slides, palm
  mutes, let ring, vibrato, dead/ghost notes, harmonics and hammer-ons/pull-offs.
- Speed from 25–150% without changing pitch, plus an optional metronome and count-in.
- Drag across bars to select and loop them.

## Controls

| Key / action | Function |
|---|---|
| Space | Play / pause |
| Click on tab | Move cursor |
| Drag on tab | Select bars and loop them |
| L / Esc | Toggle loop / clear selection |
| ← / → | Previous / next bar |
| Home | Back to start |
| + / − | Speed ±5% |
| M | Metronome |
| 1–9 | Switch track |
| Ctrl+O | Open file |

## CLI helpers

```sh
guitar-practice-tool --dump song.gp5                     # print parsed song structure
guitar-practice-tool --render song.gp5 out.wav --speed 0.75 --solo 0 --start-measure 10 --seconds 20
```
