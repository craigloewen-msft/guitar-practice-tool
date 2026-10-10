# Guitar Practice Tool

An offline, Songsterr-style tab player written in Rust. It loads Guitar Pro files
(`.gp3`, `.gp4`, `.gp5`), renders the tab in a dark theme, and plays it back with
real sampled guitars, bass and drums.

## Build & run

```sh
scripts/fetch-sounds.sh     # one-time: downloads ~800 MB of free sample banks into ./assets
cargo build --release
./target/release/guitar-practice-tool song.gp5   # or no argument: opens the first .gp3/4/5 in the cwd
```

You can also open files from the **Open…** button, with Ctrl+O, or by dragging a file onto the window.

## Features

- Tab view with rhythm notation, bends, slides, hammer-ons/pull-offs, vibrato,
  palm-mute/let-ring runs, ghost/tied notes, harmonics, repeats and tempo changes.
- Track list (guitars, bass, drums) with per-track mute/solo, volume and tone preset.
  The view shows the selected track, but every track plays together.
- Sampled sound: recorded electric guitar through real amps (distortion, crunch,
  clean), plus the dry DI guitar through a built-in amp simulator, finger/picked
  bass and an acoustic drum kit. The sampler pitch-bends the recordings to play
  bends, slides, vibrato and hammer-ons/pull-offs. It filters and shortens notes
  for palm mutes, dead notes and harmonics. If the sample banks are missing, it
  falls back to a Karplus–Strong synth.
- Tempo slider in BPM (25–150% of the score tempo) without changing pitch. It
  shows the original BPM and has a **Reset** button. There is also an optional
  metronome and count-in.
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
| + / − | Tempo ±5 BPM |
| M | Metronome |
| 1–9 | Switch track |
| Ctrl+O | Open file |

## Sound banks

The app looks for the `.sf2` banks (searched recursively) in `$GPT_SOUNDS_DIR`, `./assets`,
`assets/` next to the executable, and `~/.local/share/guitar-practice-tool`. It loads them in
the background when a song opens, using about 700 MB of RAM for a typical guitars + bass +
drums song. You can change each track's sound in the **Tone** menu of the track list.

| Tone | Bank |
|---|---|
| Distortion / Crunch / Clean | FreePats *EGuitarFSBS* dist2 / dist1 / clean (CC0) |
| Distortion / Crunch (amp sim), Acoustic | FreePats *EGuitarFSBS* direct (DI) + built-in amp model (CC0) |
| Bass (pick) / Bass (finger) | FreePats *PickedBassYR* / *FingerBassYR* (CC0) |
| Drums | *MuldjordKit* by Lars Muldjord (drumgizmo.org), SF2 by FreePats — CC-BY 4.0 |

All banks come from [freepats.zenvoid.org](https://freepats.zenvoid.org/). The license files ship inside each downloaded folder.

## CLI helpers

```sh
guitar-practice-tool --dump song.gp5                     # print parsed song structure
guitar-practice-tool --render song.gp5 out.wav --speed 0.75 --solo 0 --start-measure 10 --seconds 20
#   --tone "crunch(ampsim)"  override guitar tone   --synth  use the fallback synth instead of samples
```
