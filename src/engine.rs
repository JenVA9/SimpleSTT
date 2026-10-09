//! Config, engine/model installation (all under %LOCALAPPDATA%\SimpleSTT) and the whisper.cpp server.
//!
//! The speech engine is the official whisper.cpp `whisper-server.exe`, downloaded once (SHA-256 pinned)
//! and kept running with the model resident in VRAM, so each dictation costs ~a few hundred ms.
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU16, AtomicU32, Ordering::*};
use std::time::{Duration, Instant};

use crate::{net, util};

// ---- shared state -----------------------------------------------------------------------------
pub const IDLE: u8 = 0;
pub const REC: u8 = 1;
pub const TRANS: u8 = 2;
pub const DONE: u8 = 3;

pub static STATE: AtomicU8 = AtomicU8::new(IDLE);
pub static READY: AtomicBool = AtomicBool::new(false);
pub static FAILED: AtomicBool = AtomicBool::new(false);
pub static STATUS_BAD: AtomicBool = AtomicBool::new(false);
/// 0..=1000 download progress, -1 = none/indeterminate.
pub static PROGRESS: AtomicI32 = AtomicI32::new(-1);
static GEN: AtomicU32 = AtomicU32::new(0);
static PORT: AtomicU16 = AtomicU16::new(0);
static STATUS: Mutex<String> = Mutex::new(String::new());
static SERVER: Mutex<Option<Child>> = Mutex::new(None);
/// Only one setup run at a time: a cancelled run must fully stop before the next touches the files.
static SETUP_LOCK: Mutex<()> = Mutex::new(());

pub fn status() -> String {
    STATUS.lock().map(|s| s.clone()).unwrap_or_default()
}

pub fn set_status(s: &str, bad: bool) {
    if let Ok(mut g) = STATUS.lock() {
        *g = s.to_string();
    }
    STATUS_BAD.store(bad, Relaxed);
    util::refresh();
}

/// Status updates from the background setup only show while idle and while still current.
fn setup_status(gen: u32, s: &str) {
    if GEN.load(Relaxed) == gen && STATE.load(Relaxed) == IDLE {
        set_status(s, false);
    }
}

// ---- config -----------------------------------------------------------------------------------
/// (name, approx. download size in MB). `.en` models are English-only and a bit more accurate.
pub const MODELS: &[(&str, u32)] = &[
    ("tiny.en", 75),
    ("base.en", 142),
    ("small.en", 466),
    ("medium.en", 1533),
    ("tiny", 75),
    ("base", 142),
    ("small", 466),
    ("medium", 1533),
    ("large-v3-turbo-q5_0", 574),
    ("large-v3-turbo", 1624),
    ("large-v3", 3095),
];

#[derive(Clone, PartialEq)]
pub struct Config {
    pub models_dir: PathBuf,
    pub model: String,
    pub lang: String,
    /// also capture what the PC is playing (calls, videos) and mix it with the mic
    pub desktop: bool,
}

impl Config {
    /// Settings that require restarting the whisper server when changed.
    pub fn engine_differs(&self, o: &Config) -> bool {
        self.models_dir != o.models_dir || self.model != o.model || self.lang != o.lang
    }
}

impl Config {
    fn file() -> PathBuf {
        util::data_dir().join("config.ini")
    }

    pub fn load() -> Config {
        let mut c = Config {
            models_dir: util::data_dir().join("models"),
            model: "large-v3-turbo-q5_0".into(),
            lang: "auto".into(),
            desktop: false,
        };
        if let Ok(t) = std::fs::read_to_string(Self::file()) {
            for l in t.lines() {
                if let Some((k, v)) = l.split_once('=') {
                    let v = v.trim();
                    match k.trim() {
                        "models_dir" if !v.is_empty() => c.models_dir = PathBuf::from(v),
                        "model" if MODELS.iter().any(|m| m.0 == v) => c.model = v.into(),
                        "lang" if !v.is_empty() => c.lang = v.into(),
                        "desktop" => c.desktop = v == "1",
                        _ => {}
                    }
                }
            }
        }
        c
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(util::data_dir());
        let t = format!(
            "models_dir={}\nmodel={}\nlang={}\ndesktop={}\n",
            self.models_dir.display(),
            self.model,
            self.lang,
            self.desktop as u8
        );
        let _ = std::fs::write(Self::file(), t);
    }

    pub fn model_path(&self) -> PathBuf {
        model_path_in(&self.models_dir, &self.model)
    }
}

pub fn model_path_in(dir: &Path, model: &str) -> PathBuf {
    dir.join(format!("ggml-{model}.bin"))
}

// ---- engine install ---------------------------------------------------------------------------
const REL: &str = "https://github.com/ggml-org/whisper.cpp/releases/download/b5130/";
// (variant dir, asset, sha256, size MB). Pinned: known-good with whisper-server + Blackwell PTX.
const CUDA: (&str, &str, &str, u32) = (
    "cuda",
    "whisper-cublas-12.4.0-bin-x64.zip",
    "af520ddd034d985b55dfeea3e465ed93653ba2aee1a55e865033edc548c272a7",
    643,
);
const CPU: (&str, &str, &str, u32) = (
    "cpu",
    "whisper-bin-x64.zip",
    "f9ec6c52a2e949b62ab51fa21d0d497958f9e41c3010c157c4e42932d5316f3c",
    8,
);

fn has_nvidia() -> bool {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;
    unsafe { !LoadLibraryA(b"nvcuda.dll\0".as_ptr()).is_null() }
}

fn stale(gen: u32) -> bool {
    GEN.load(Relaxed) != gen
}

fn dl(gen: u32, url: &str, dest: &Path, label: &str) -> Result<(), String> {
    let mut last = -1i32;
    net::download(url, dest, &mut |done, total| {
        if total > 0 {
            let p = (done * 1000 / total) as i32;
            if p != last {
                last = p;
                PROGRESS.store(p, Relaxed);
                setup_status(gen, &format!("{label} {}%", p / 10));
            }
        }
        !stale(gen)
    })
}

fn ensure_engine(gen: u32) -> Result<PathBuf, String> {
    let v = if has_nvidia() { CUDA } else { CPU };
    let root = util::data_dir().join("engine");
    let dir = root.join(v.0);
    let exe = dir.join("Release").join("whisper-server.exe");
    if exe.exists() {
        return Ok(exe);
    }
    let zip = root.join(v.1);
    let label = if v.0 == "cuda" { "Downloading GPU engine" } else { "Downloading engine" };
    dl(gen, &format!("{REL}{}", v.1), &zip, label)?;
    PROGRESS.store(-1, Relaxed);
    setup_status(gen, "Verifying engine…");
    let sha = util::sha256_file(&zip).unwrap_or_default();
    if sha != v.2 {
        let _ = std::fs::remove_file(&zip);
        return Err("Engine download was corrupt — click to retry".into());
    }
    setup_status(gen, "Unpacking engine…");
    let tmp = root.join(format!("{}.tmp", v.0));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    // tar.exe (bsdtar) ships with Windows 10 1803+ and unzips natively.
    let tar = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .unwrap_or_else(|| PathBuf::from("tar.exe"));
    let ok = Command::new(tar)
        .arg("-xf")
        .arg(&zip)
        .arg("-C")
        .arg(&tmp)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok || !tmp.join("Release").join("whisper-server.exe").exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("Couldn't unpack the engine".into());
    }
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::rename(&tmp, &dir).map_err(|e| format!("Couldn't install engine: {e}"))?;
    let _ = std::fs::remove_file(&zip);
    Ok(exe)
}

fn ensure_model(gen: u32, cfg: &Config) -> Result<PathBuf, String> {
    let p = cfg.model_path();
    if p.exists() {
        return Ok(p);
    }
    PROGRESS.store(0, Relaxed);
    let url = format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-{}.bin",
        cfg.model
    );
    dl(gen, &url, &p, "Downloading model")?;
    PROGRESS.store(-1, Relaxed);
    Ok(p)
}

// ---- server -----------------------------------------------------------------------------------
fn job() -> isize {
    use windows_sys::Win32::System::JobObjects::*;
    static JOB: std::sync::OnceLock<isize> = std::sync::OnceLock::new();
    *JOB.get_or_init(|| unsafe {
        let j = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if !j.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                j,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            );
        }
        j as isize
    })
}

pub fn kill_server() {
    READY.store(false, Relaxed);
    if let Ok(mut g) = SERVER.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn launch(exe: &Path, model: &Path, cfg: &Config, gen: u32) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_err(|e| e.to_string())?
        .port();
    PORT.store(port, Relaxed);
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let gpu = exe.to_string_lossy().contains("\\cuda\\");
    // GPU does the heavy lifting; extra CPU threads only help the CPU build.
    let threads = if gpu { (cores / 2).clamp(2, 8) } else { cores.max(2) };
    let lang = if cfg.model.ends_with(".en") { "en" } else { cfg.lang.as_str() };

    let child = Command::new(exe)
        .args(["-m"])
        .arg(model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        .args(["-t", &threads.to_string(), "-l", lang, "-sns", "-nth", "0.6"])
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        // keep the CUDA JIT cache inside our data dir (self-contained, and big enough to stay warm)
        .env("CUDA_CACHE_PATH", util::data_dir().join("cudacache"))
        .env("CUDA_CACHE_MAXSIZE", "4294967296")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Couldn't start engine: {e}"))?;
    unsafe { AssignProcessToJobObject(job() as _, child.as_raw_handle() as _) };
    *SERVER.lock().unwrap() = Some(child);

    setup_status(gen, "Loading model…");
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let t0 = Instant::now();
    loop {
        if stale(gen) {
            return Err("cancelled".into());
        }
        if let Some(c) = SERVER.lock().unwrap().as_mut() {
            if let Ok(Some(_)) = c.try_wait() {
                return Err("Engine crashed on start — try another model".into());
            }
        }
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if t0.elapsed() > Duration::from_secs(300) {
            return Err("Engine took too long to start".into());
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn setup(cfg: &Config, gen: u32) -> Result<(), String> {
    setup_status(gen, "Starting…");
    let exe = ensure_engine(gen)?;
    let model = ensure_model(gen, cfg)?;
    PROGRESS.store(-1, Relaxed);
    launch(&exe, &model, cfg, gen)?;
    // Warm-up: the first GPU call on a brand-new architecture JIT-compiles kernels (tens of seconds,
    // cached afterwards). Do it now, in the background, instead of on the user's first dictation.
    setup_status(gen, "Warming up GPU…");
    let silence = vec![0i16; 16000];
    transcribe_raw(&silence, Duration::from_secs(600))?;
    if !stale(gen) {
        READY.store(true, Relaxed);
        setup_status(gen, "Ready — click the mic");
        util::refresh();
    }
    Ok(())
}

/// (Re)starts the whole setup pipeline on a background thread; stale runs abort themselves.
pub fn start_setup(cfg: Config) {
    kill_server();
    let gen = GEN.fetch_add(1, Relaxed) + 1;
    FAILED.store(false, Relaxed);
    PROGRESS.store(-1, Relaxed);
    std::thread::spawn(move || {
        let _one_at_a_time = SETUP_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if stale(gen) {
            return;
        }
        if let Err(e) = setup(&cfg, gen) {
            if !stale(gen) {
                FAILED.store(true, Relaxed);
                PROGRESS.store(-1, Relaxed);
                set_status(&e, true);
            }
        }
    });
}

/// Abort any running setup/download and stop the server (used before wiping data).
pub fn cancel() {
    GEN.fetch_add(1, Relaxed);
    kill_server();
}

#[cfg(test)]
pub fn test_set_port(p: u16) {
    PORT.store(p, Relaxed);
}

// ---- transcription ----------------------------------------------------------------------------
fn wav(samples: &[i16]) -> Vec<u8> {
    let data = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&16000u32.to_le_bytes());
    v.extend_from_slice(&32000u32.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

fn transcribe_raw(samples: &[i16], timeout: Duration) -> Result<String, String> {
    const B: &str = "----SimpleSTTboundary7MA4YWxk";
    let mut body = Vec::new();
    for (k, v) in [("temperature", "0.0"), ("response_format", "json")] {
        body.extend_from_slice(
            format!("--{B}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&wav(samples));
    body.extend_from_slice(format!("\r\n--{B}--\r\n").as_bytes());

    let port = PORT.load(Relaxed);
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|_| "engine not running".to_string())?;
    let _ = s.set_read_timeout(Some(timeout));
    let head = format!(
        "POST /inference HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: multipart/form-data; boundary={B}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())
        .and_then(|_| s.write_all(&body))
        .map_err(|_| "engine not running".to_string())?;
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp);
    let resp = String::from_utf8_lossy(&resp);
    if !resp.starts_with("HTTP/1.1 200") {
        return Err("engine error".into());
    }
    util::json_str(&resp, "text").ok_or_else(|| "engine error".to_string())
}

/// Collapses whitespace and drops Whisper's non-speech placeholders like "[BLANK_AUDIO]".
fn clean(t: &str) -> String {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let blank = (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'));
    if blank { String::new() } else { t }
}

pub fn transcribe(samples: &[i16]) -> Result<String, String> {
    transcribe_raw(samples, Duration::from_secs(300)).map(|t| clean(&t))
}
