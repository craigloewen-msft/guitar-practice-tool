# Guitar Practice Tool

An offline Guitar Pro (`.gp3`/`.gp4`/`.gp5`) tab player for practicing. You can click the tab and press **Space** to play, switch tracks, mute or solo each track, loop bars, and slow down or speed up the tempo. Every guitar is a synthesized string played through a [Neural Amp Modeler](https://github.com/sdatkinson/NeuralAmpModelerPlugin) capture, using a WaveNet engine written in pure Rust. Bass and drums use samples.

## Install

Download the installer for your OS from [Releases](../../releases): `.exe` (Windows), `.dmg` (macOS), `.deb` or `.AppImage` (Linux).
The builds are unsigned. On macOS, right-click the app and choose **Open** the first time. On Windows, click **More info → Run anyway**.

## First run

1. Click **Download sounds** (about 220 MB of free bass and drum samples).
2. Click **Choose .nam…** and pick an amp capture, for example a free one from [TONE3000](https://www.tone3000.com).
3. Open a tab file, or drag it onto the window.

## Build from source

```sh
cargo run --release -- song.gp5
```

Linux needs `libasound2-dev` and `pkg-config`. To make installers, run `cargo install cargo-packager`, then `cargo build --release && cargo packager --release`.

If audio stutters, run with `GPT_TRACE=1` to print engine load and buffer underruns once a second.

## License

The code is dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
Sounds are downloaded at runtime and are not bundled. They come from [FreePats](https://freepats.zenvoid.org/) (CC0; MuldjordKit drums are CC-BY 4.0).
