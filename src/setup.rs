//! App data locations, persisted settings and the first-run sound download.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::synth::sampler::{BankId, Library};

fn project_dirs() -> Option<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "guitar-practice-tool")
}

/// Where downloaded sounds and the chosen amp model live.
pub fn data_dir() -> Option<PathBuf> {
    project_dirs().map(|p| p.data_local_dir().to_path_buf())
}

fn settings_path() -> Option<PathBuf> {
    project_dirs().map(|p| p.config_dir().join("settings.json"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub recent: Vec<PathBuf>,
    pub nam_model: Option<PathBuf>,
    pub master: f32,
    pub metronome: bool,
    pub count_in: bool,
    pub follow: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { recent: Vec::new(), nam_model: None, master: 0.8, metronome: false, count_in: false, follow: true }
    }
}

impl Settings {
    pub fn load() -> Settings {
        settings_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = settings_path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(p, json);
        }
    }

    pub fn push_recent(&mut self, path: &Path) {
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(8);
    }
}

/// Copy a user-chosen `.nam` capture into the data dir so it survives the original moving.
pub fn install_nam_model(src: &Path) -> Result<PathBuf> {
    let dir = data_dir().context("no data directory")?.join("nam");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(src.file_name().context("bad file name")?);
    if src.canonicalize().ok() != dest.canonicalize().ok() {
        std::fs::copy(src, &dest).with_context(|| format!("copy {}", src.display()))?;
    }
    Ok(dest)
}

/// FreePats SF2 archives (CC0 / CC-BY 4.0) for bass and drums. Guitars need no samples:
/// they are synthesized and played through the NAM capture.
const ARCHIVES: &[(BankId, &str)] = &[
    (BankId::BassFinger, "https://github.com/freepats/electric-bass-YR/releases/download/2019-09-30/FingerBassYR-SF2-20190930.7z"),
    (BankId::BassPicked, "https://github.com/freepats/electric-bass-YR/releases/download/2019-09-30/PickedBassYR-SF2-20190930.7z"),
    (BankId::Drums, "https://github.com/freepats/muldjordkit/releases/download/2020-10-18/MuldjordKit-SF2-20201018.7z"),
];

/// Banks the download still has to fetch.
pub fn missing(lib: &Library) -> Vec<BankId> {
    ARCHIVES.iter().map(|(id, _)| *id).filter(|&id| lib.find_file(id).is_none()).collect()
}

pub fn sounds_installed(lib: &Library) -> bool {
    missing(lib).is_empty()
}

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub finished: Option<Result<(), String>>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 { 0.0 } else { (self.done as f64 / self.total as f64) as f32 }
    }
}

pub type SharedProgress = Arc<Mutex<Progress>>;

/// Download and unpack all missing sounds on a background thread.
pub fn start_download(lib: Arc<Library>, on_done: impl FnOnce() + Send + 'static) -> SharedProgress {
    let progress: SharedProgress = Arc::new(Mutex::new(Progress { stage: "Starting…".into(), ..Default::default() }));
    let p = progress.clone();
    std::thread::spawn(move || {
        let r = download_all(&lib, &p).map_err(|e| format!("{e:#}"));
        p.lock().unwrap().finished = Some(r);
        on_done();
    });
    progress
}

fn download_all(lib: &Library, progress: &SharedProgress) -> Result<()> {
    let dest = data_dir().context("no data directory")?;
    std::fs::create_dir_all(&dest)?;
    let banks = missing(lib);
    let steps = banks.len();
    let agent = ureq::Agent::new_with_defaults();
    let mut step = 0;
    for id in banks {
        step += 1;
        let url = ARCHIVES.iter().find(|(b, _)| *b == id).unwrap().1;
        let stage = format!("Downloading {} ({step}/{steps})", id.label());
        let tmp = dest.join(format!(".{}.7z.part", id.file_prefix()));
        fetch(&agent, url, &tmp, progress, &stage)?;
        set(progress, format!("Unpacking {} ({step}/{steps})", id.label()), 0, 0);
        let r = sevenz_rust2::decompress_file(&tmp, &dest).with_context(|| format!("unpack {url}"));
        let _ = std::fs::remove_file(&tmp);
        r?;
    }
    set(progress, "Done".into(), 1, 1);
    lib.rescan();
    Ok(())
}

fn set(p: &SharedProgress, stage: String, done: u64, total: u64) {
    let mut g = p.lock().unwrap();
    g.stage = stage;
    g.done = done;
    g.total = total;
}

fn fetch(agent: &ureq::Agent, url: &str, out: &Path, progress: &SharedProgress, stage: &str) -> Result<()> {
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    let total = resp.body().content_length().unwrap_or(0);
    let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
    let mut file = std::fs::File::create(out)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut done = 0u64;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])?;
        done += n as u64;
        set(progress, stage.to_string(), done, total);
    }
    if total > 0 && done != total {
        bail!("download of {url} was cut short");
    }
    Ok(())
}
