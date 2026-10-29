# Guitar Practice Tool

This project was entirely vibe coded - so use at your own discretion.

An offline Guitar Pro (`.gp3`/`.gp4`/`.gp5`) tab player for practicing. You can click the tab and press **Space** to play, switch tracks, mute or solo each track, loop bars, and slow down or speed up the tempo. With a loop set, pausing and playing again starts you back at the top of the loop. Every guitar is a synthesized string played through a [Neural Amp Modeler](https://github.com/sdatkinson/NeuralAmpModelerPlugin) capture, using a WaveNet engine written in pure Rust. Bass and drums use samples.

## Install

Download the installer for your OS from [Releases](../../releases): `.exe` (Windows), `.dmg` (macOS), `.deb` or `.AppImage` (Linux).
The builds are unsigned. On macOS, right-click the app and choose **Open** the first time. On Windows, click **More info → Run anyway**.

## First run

1. Click **Download sounds** (about 220 MB of free bass and drum samples).
2. Click **Choose .nam…** and pick an amp capture, for example a free one from [TONE3000](https://www.tone3000.com).
3. Open a tab file, or drag it onto the window.

## Playing along with the record

You can attach the real recording to a tab and practice against the band instead of the synth.

1. Open a tab, then click **♫ Add audio…** under **Song audio** in the sidebar.
2. Pick the audio. Separated stems work best — one file per instrument, as produced by a stem splitter — but a single full mix also works. Everything you pick must come from the same master, since the app lines them all up with one measurement.
3. The app lines the whole tab up against the recording, checks it hit by hit every couple of seconds through the song, and reports what it found. Press **S** to switch between **Synth** and **Song**.

The recording is the source of truth. Alignment first matches the song's shape: where the drums, bass and guitar come in and drop out, and which chords are playing. A band keeps fairly steady time, so the tab's tempo is only allowed to change gradually, or at the tab's own tempo changes. That is what stops a riff that repeats bar after bar from landing a few bars out. Each stretch is then lined up on the individual hits. In **Song** mode the recording plays at its own pace, and the tab's cursor, metronome and loop points follow it. The recording is never sped up or slowed down to fit the tab.

Nothing assumes the recording starts or ends with the tab. A count-in, an extra intro, a fade-out or a long tail are all fine: where bar 1 falls is a free measurement, and the report tells you how much lead-in and tail it found.

With stems you can mute the guitar and play its part yourself while the rest of the band keeps going. Mute, solo and volume work per stem, and the choice of audio is remembered for that tab.

**Details…** opens the full report: the range of tempos the band plays at against the tab, every check through the song with how far lining up the hits nudged it, and how closely each stem's hits land on the tab's. That last figure is in milliseconds rather than a percentage, because milliseconds can be judged by ear — under about 40ms the tab and the record sound like one performance. It is quoted from the part with the sharpest attack, usually the drums, since that is the only kind of sound that can pin down a beat; bass, chords and voices swell in rather than strike, so they measure worse without being lined up any differently. Every part shares one alignment. A tempo other than *original* time-stretches the recording, so it plays slower or faster at its original pitch. If the recording is a different take from the tab, alignment fails and says so rather than playing something that does not fit. If it is only slightly out, **Nudge by hand** shifts the whole recording in 20ms steps and remembers it for that tab.

To print the alignment measurements, run `guitar-practice-tool --align <tab> <audio>...` with `GPT_ALIGN_DEBUG=1`. Add `GPT_ALIGN_BARS=1` for each bar's time in the recording.

## Build from source

```sh
cargo run --release -- song.gp5
```

Linux needs `libasound2-dev` and `pkg-config`. To make installers, run `cargo install cargo-packager`, then `cargo build --release && cargo packager --release`.

If audio stutters, run with `GPT_TRACE=1` to print engine load and buffer underruns once a second.

## License

The code is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Sounds are downloaded at runtime and are not bundled. They come from [FreePats](https://freepats.zenvoid.org/) (CC0; MuldjordKit drums are CC-BY 4.0).
