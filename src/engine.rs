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

use crate::llm::AiMode;
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
/// true while the loaded engine is the IPA (POWSM) python server instead of whisper-server
static IPA: AtomicBool = AtomicBool::new(false);
/// Set by the UI once the user accepted the "extra download" warning. Without it, setup refuses to fetch
/// the IPA runtime/model, so a stray config edit can never trigger a multi-GB download.
pub static IPA_CONSENT: AtomicBool = AtomicBool::new(false);

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
pub(crate) fn setup_status(gen: u32, s: &str) {
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
    // IPA phone recognition (POWSM). Needs an extra Python/PyTorch runtime that is only fetched on demand.
    (IPA_MODEL, 1311),
];

pub const IPA_MODEL: &str = "powsm-ipa";
pub fn is_ipa(model: &str) -> bool {
    model == IPA_MODEL
}

/// Model name meaning "use the file in `custom_model` / `custom_text`" (any ggml .bin / .gguf the user picked).
pub const CUSTOM: &str = "custom";

#[derive(Clone, PartialEq)]
pub struct Config {
    pub models_dir: PathBuf,
    pub model: String,
    pub lang: String,
    /// also capture what the PC is playing (calls, videos) and mix it with the mic
    pub desktop: bool,
    /// text AI (Gemma): where its models live, which one, and what it does (off / simple / rambler / cliff)
    pub text_dir: PathBuf,
    pub text_model: String,
    pub ai_mode: AiMode,
    /// user-picked model files for model == CUSTOM / text_model == CUSTOM
    pub custom_model: PathBuf,
    pub custom_text: PathBuf,
    /// false until the first-run wizard has been completed (never shown to existing installs)
    pub onboarded: bool,
}

impl Config {
    /// Settings that require restarting the engine(s) when changed.
    pub fn engine_differs(&self, o: &Config) -> bool {
        self.models_dir != o.models_dir
            || self.model != o.model
            || self.lang != o.lang
            || self.custom_model != o.custom_model
            // simple <-> rambler <-> cliff only changes prompts; turning the AI on/off or swapping its model restarts
            || ((self.ai_mode == AiMode::Off) != (o.ai_mode == AiMode::Off))
            || (self.ai_mode != AiMode::Off
                && (self.text_dir != o.text_dir || self.text_model != o.text_model || self.custom_text != o.custom_text))
    }
}

/// Anything on disk from an earlier version means this is an existing install, not a first run.
fn existing_install() -> bool {
    let d = util::data_dir();
    d.join("config.ini").exists() || d.join("engine").exists() || d.join("models").exists()
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
            text_dir: util::data_dir().join("text-models"),
            text_model: crate::llm::TEXT_MODELS[crate::llm::recommended_text()].id.into(),
            ai_mode: AiMode::Off,
            custom_model: PathBuf::new(),
            custom_text: PathBuf::new(),
            // installs from before onboarding existed (and fresh updates of them) skip the wizard
            onboarded: existing_install(),
        };
        let (mut ai_set, mut legacy_ipa_ai) = (false, true);
        if let Ok(t) = std::fs::read_to_string(Self::file()) {
            for l in t.lines() {
                if let Some((k, v)) = l.split_once('=') {
                    let v = v.trim();
                    match k.trim() {
                        "models_dir" if !v.is_empty() => c.models_dir = PathBuf::from(v),
                        "model" if v == CUSTOM || MODELS.iter().any(|m| m.0 == v) => c.model = v.into(),
                        "text_dir" if !v.is_empty() => c.text_dir = PathBuf::from(v),
                        "text_model" if v == CUSTOM || crate::llm::TEXT_MODELS.iter().any(|m| m.id == v) => c.text_model = v.into(),
                        "ai_mode" => {
                            if let Some(m) = AiMode::parse(v) {
                                c.ai_mode = m;
                                ai_set = true;
                            }
                        }
                        "ipa_ai" => legacy_ipa_ai = v != "0", // configs from before the mode selector
                        "custom_model" => c.custom_model = PathBuf::from(v),
                        "custom_text" => c.custom_text = PathBuf::from(v),
                        "onboarded" => c.onboarded = v == "1",
                        "lang" if !v.is_empty() => c.lang = v.into(),
                        "desktop" => c.desktop = v == "1",
                        _ => {}
                    }
                }
            }
        }
        if !ai_set {
            // old configs: IPA used the AI unless switched off; normal dictation never used it
            c.ai_mode = if is_ipa(&c.model) && legacy_ipa_ai { AiMode::Simple } else { AiMode::Off };
        }
        c
    }

    pub fn save(&self) {
        let _ = std::fs::create_dir_all(util::data_dir());
        let t = format!(
            "models_dir={}\nmodel={}\nlang={}\ndesktop={}\ntext_dir={}\ntext_model={}\nai_mode={}\ncustom_model={}\ncustom_text={}\nonboarded={}\n",
            self.models_dir.display(),
            self.model,
            self.lang,
            self.desktop as u8,
            self.text_dir.display(),
            self.text_model,
            self.ai_mode.id(),
            self.custom_model.display(),
            self.custom_text.display(),
            self.onboarded as u8
        );
        let _ = std::fs::write(Self::file(), t);
    }

    pub fn model_path(&self) -> PathBuf {
        if self.model == CUSTOM {
            return self.custom_model.clone();
        }
        model_path_in(&self.models_dir, &self.model)
    }
}

pub fn model_path_in(dir: &Path, model: &str) -> PathBuf {
    if is_ipa(model) {
        // the big checkpoint; the other POWSM files sit beside it (see powsm_files)
        return dir.join("powsm").join(POWSM_EXP).join("valid.acc.ave_5best.till45epoch.pth");
    }
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

pub fn has_nvidia() -> bool {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;
    unsafe { !LoadLibraryA(b"nvcuda.dll\0".as_ptr()).is_null() }
}

pub(crate) fn stale(gen: u32) -> bool {
    GEN.load(Relaxed) != gen
}

pub(crate) fn dl(gen: u32, url: &str, dest: &Path, label: &str) -> Result<(), String> {
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
pub(crate) fn job() -> isize {
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
    crate::llm::kill_server();
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
    wait_ready(port, gen, "Engine crashed on start — try another model")
}

/// Waits until the freshly spawned server accepts connections (it only listens once the model is loaded).
fn wait_ready(port: u16, gen: u32, crash_msg: &str) -> Result<(), String> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let t0 = Instant::now();
    loop {
        if stale(gen) {
            return Err("cancelled".into());
        }
        if let Some(c) = SERVER.lock().unwrap().as_mut() {
            if let Ok(Some(_)) = c.try_wait() {
                return Err(crash_msg.into());
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
    IPA.store(is_ipa(&cfg.model), Relaxed);
    crate::llm::set_mode(cfg.ai_mode);
    // longest chunk before a forced cut; shorter IPA chunks were measured (6 s / 10 s) and did NOT help,
    // so it stays 20 s. SIMPLESTT_IPA_CHUNK_S is a test knob for that experiment.
    let chunk_s = std::env::var("SIMPLESTT_IPA_CHUNK_S").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    crate::live::MAX_CHUNK_S.store(if is_ipa(&cfg.model) { chunk_s } else { 20 }, Relaxed);
    if is_ipa(&cfg.model) {
        return setup_ipa(cfg, gen);
    }
    let exe = ensure_engine(gen)?;
    let model = ensure_model(gen, cfg)?;
    let text = ensure_text(gen, cfg)?;
    PROGRESS.store(-1, Relaxed);
    launch(&exe, &model, cfg, gen)?;
    if let Some((llama, gguf)) = text {
        crate::llm::launch(&llama, &gguf, gen)?;
    }
    // Warm-up: the first GPU call on a brand-new architecture JIT-compiles kernels (tens of seconds,
    // cached afterwards). Do it now, in the background, instead of on the user's first dictation.
    setup_status(gen, "Warming up GPU…");
    let silence = vec![0i16; 16000];
    transcribe_raw(&silence, Duration::from_secs(600))?;
    crate::llm::warm(); // first request allocates GPU buffers; do it now, not on the first dictation
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
    let (path, ctype, body) = if IPA.load(Relaxed) {
        // IPA server: raw 16 kHz mono s16le PCM in, {"text": "..."} out
        let mut raw = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        ("/ipa", "application/octet-stream".to_string(), raw)
    } else {
        ("/inference", format!("multipart/form-data; boundary={B}"), whisper_form(samples, B))
    };
    send_request(path, &ctype, &body, timeout)
}

#[allow(non_snake_case)]
fn whisper_form(samples: &[i16], boundary: &str) -> Vec<u8> {
    let B = boundary;
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
    body
}

fn send_request(path: &str, ctype: &str, body: &[u8], timeout: Duration) -> Result<String, String> {
    let resp = http_post(PORT.load(Relaxed), path, ctype, body, timeout)?;
    util::json_str(&resp, "text").ok_or_else(|| "engine error".to_string())
}

/// POST to a local engine; returns the raw response (headers + body) if the status was 200.
pub(crate) fn http_post(port: u16, path: &str, ctype: &str, body: &[u8], timeout: Duration) -> Result<String, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|_| "engine not running".to_string())?;
    let _ = s.set_read_timeout(Some(timeout));
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes())
        .and_then(|_| s.write_all(body))
        .map_err(|_| "engine not running".to_string())?;
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp);
    let resp = String::from_utf8_lossy(&resp).into_owned();
    if resp.split_whitespace().nth(1) != Some("200") {
        return Err("engine error".into());
    }
    Ok(resp)
}

/// Collapses whitespace and drops Whisper's non-speech placeholders like "[BLANK_AUDIO]".
fn clean(t: &str) -> String {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let blank = (t.starts_with('[') && t.ends_with(']')) || (t.starts_with('(') && t.ends_with(')'));
    if blank { String::new() } else { t }
}

pub fn transcribe(samples: &[i16]) -> Result<String, String> {
    let t = clean(&transcribe_raw(samples, Duration::from_secs(300))?);
    let mode = crate::llm::mode();
    if t.is_empty() || mode == AiMode::Off {
        return Ok(t);
    }
    // If the text engine hiccups, the raw text/phones are still better than nothing.
    if IPA.load(Relaxed) {
        // every AI mode first turns the phones into words, chunk by chunk
        return Ok(crate::llm::decode_ipa(&t).map(|f| clean(&f)).unwrap_or(t));
    }
    if mode == AiMode::Simple {
        return Ok(crate::llm::grammar(&t).map(|f| clean(&f)).unwrap_or(t));
    }
    Ok(t) // rambler / cliffnotes work on whole groups of text later (llm::acc_*)
}

// ---- IPA (POWSM) --------------------------------------------------------------------------------
// Phone recognition runs on the official POWSM checkpoint (ESPnet/PyTorch), which whisper.cpp can't load.
// So it needs a private Python + PyTorch + ESPnet runtime. That runtime (~4.3 GB with CUDA libraries) is
// NEVER fetched at startup: only after the user selects the IPA model and accepts the warning dialog
// (see `IPA_CONSENT`). Everything lives under %LOCALAPPDATA%\SimpleSTT\ipa and is removed by "Delete all data".
const POWSM_REV: &str = "21ffa410432ace159f3c1fdeed304c70eddf7d34"; // pinned commit of espnet/powsm
const POWSM_EXP: &str = "exp/s2t_train_s2t_ebf_conv2d_size768_e9_d9_piecewise_lr5e-4_warmup60k_flashattn_raw_bpe40000";
const UV_URL: &str = "https://github.com/astral-sh/uv/releases/download/0.13.1/uv-x86_64-pc-windows-msvc.zip";
const UV_SHA: &str = "3b2661a463a5baea7f5076ea2b25ab71d77f36a6c0d809e5350651792f80a672";
const IPA_MARK: &str = "ready-1"; // bump to force a runtime reinstall when the pinned packages change
pub const IPA_RUNTIME_MB: u32 = 4300; // CUDA build; the CPU build is much smaller
pub const IPA_RUNTIME_CPU_MB: u32 = 800;

/// (path inside the repo, sha256). Stored under <models folder>/powsm/ with the same layout.
fn powsm_files() -> [(String, &'static str); 4] {
    [
        (
            "exp/s2t_stats_raw_bpe40000/train/feats_stats.npz".into(),
            "f3ca2ef68be502a75a646c8da36847375964a0d6499fd9ee2d7d620a0f31d746",
        ),
        (
            "data/token_list/bpe_unigram40000/bpe.model".into(),
            "9b9a8b76353430d41a1b8f7f2ec0f40fa8c4e75567eaef6887bdbb893c55236a",
        ),
        (
            format!("{POWSM_EXP}/config.yaml"),
            "f29993179ff12d9f082052c8e1ccbe192c26f6bfe116cc53d6422c5c8012787b",
        ),
        (
            format!("{POWSM_EXP}/valid.acc.ave_5best.till45epoch.pth"),
            "a91a03bcfd59a956319939891c4098e8f7e8c9ea568d7ec2bcbc1131b32d1197",
        ),
    ]
}

fn ipa_dir() -> PathBuf {
    util::data_dir().join("ipa")
}

fn ipa_python() -> PathBuf {
    ipa_dir().join("venv").join("Scripts").join("python.exe")
}

/// Is the Python/PyTorch/ESPnet runtime installed?
pub fn ipa_runtime_ready() -> bool {
    ipa_dir().join(IPA_MARK).exists() && ipa_python().exists()
}

/// Are the POWSM model files present in `models_dir`?
pub fn powsm_model_ready(models_dir: &Path) -> bool {
    powsm_files().iter().all(|(p, _)| models_dir.join("powsm").join(p).exists())
}

/// Removes only the files SimpleSTT itself put in the models folder (never the user's other files).
pub fn powsm_remove(models_dir: &Path) {
    for (p, _) in powsm_files() {
        let _ = std::fs::remove_file(models_dir.join("powsm").join(&p));
        let _ = std::fs::remove_file(models_dir.join("powsm").join(format!("{p}.part")));
    }
    // prune now-empty folders (remove_dir fails on non-empty ones, which keeps anything else there)
    for sub in [
        POWSM_EXP,
        "exp/s2t_stats_raw_bpe40000/train",
        "exp/s2t_stats_raw_bpe40000",
        "exp",
        "data/token_list/bpe_unigram40000",
        "data/token_list",
        "data",
        "",
    ] {
        let _ = std::fs::remove_dir(models_dir.join("powsm").join(sub));
    }
}

fn dir_size(p: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_dir() => n += dir_size(&e.path()),
                Ok(m) => n += m.len(),
                _ => {}
            }
        }
    }
    n
}

pub(crate) fn unzip(zip: &Path, dest: &Path) -> bool {
    let tar = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("tar.exe"))
        .unwrap_or_else(|| PathBuf::from("tar.exe"));
    Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dest)
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Runs a helper (uv / python) hidden, logging to ipa\install.log, killing it if the setup is cancelled.
/// While it runs, the ipa folder's growth drives the progress ring (`expect` bytes = 100 %).
fn run_tool(gen: u32, mut cmd: Command, label: &str, expect: u64) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    let log = std::fs::OpenOptions::new().create(true).append(true).open(ipa_dir().join("install.log")).ok();
    let (out, err) = match log.as_ref().and_then(|f| Some((f.try_clone().ok()?, f.try_clone().ok()?))) {
        Some((a, b)) => (Stdio::from(a), Stdio::from(b)),
        None => (Stdio::null(), Stdio::null()),
    };
    let dir = ipa_dir();
    cmd.env("UV_PYTHON_INSTALL_DIR", dir.join("python"))
        .env("UV_CACHE_DIR", dir.join("uvcache"))
        .env("UV_PYTHON_PREFERENCE", "only-managed")
        .env("UV_LINK_MODE", "copy")
        .env("UV_NO_PROGRESS", "1")
        .env("NLTK_DATA", dir.join("nltk_data"))
        .env("PYTHONUTF8", "1")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    let mut child = cmd.spawn().map_err(|e| format!("{label}: {e}"))?;
    unsafe { AssignProcessToJobObject(job() as _, child.as_raw_handle() as _) };
    let mut tick = 0u32;
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(_)) => return Err(format!("{label} failed — see ipa\\install.log")),
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        if stale(gen) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cancelled".into());
        }
        if tick % 4 == 0 && expect > 0 {
            let p = ((dir_size(&dir) * 1000 / expect) as i32).min(990);
            PROGRESS.store(p, Relaxed);
            setup_status(gen, &format!("{label} {}%", p / 10));
        }
        tick += 1;
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn install_ipa_runtime(gen: u32) -> Result<(), String> {
    let dir = ipa_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let uv = dir.join("uv").join("uv.exe");
    if !uv.exists() {
        let zip = dir.join("uv.zip");
        dl(gen, UV_URL, &zip, "Downloading installer")?;
        if util::sha256_file(&zip).unwrap_or_default() != UV_SHA {
            let _ = std::fs::remove_file(&zip);
            return Err("Installer download was corrupt — click to retry".into());
        }
        let tmp = dir.join("uv.tmp");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
        if !unzip(&zip, &tmp) || !tmp.join("uv.exe").exists() {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err("Couldn't unpack the installer".into());
        }
        let _ = std::fs::remove_dir_all(dir.join("uv"));
        std::fs::rename(&tmp, dir.join("uv")).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&zip);
    }
    let gpu = has_nvidia();
    // peak size of cache + environment while installing (wheels are cached, then extracted)
    let expect: u64 = (if gpu { IPA_RUNTIME_MB } else { IPA_RUNTIME_CPU_MB } as u64) * 1_048_576 * 17 / 10;
    let venv = dir.join("venv");
    let py = ipa_python();

    PROGRESS.store(-1, Relaxed);
    setup_status(gen, "Installing Python…");
    let mut c = Command::new(&uv);
    c.args(["venv", "--python", "3.11"]).arg(&venv);
    run_tool(gen, c, "Installing Python", 0)?;

    let index = if gpu {
        "https://download.pytorch.org/whl/cu128"
    } else {
        "https://download.pytorch.org/whl/cpu"
    };
    let mut c = Command::new(&uv);
    c.args(["pip", "install", "--python"])
        .arg(&py)
        .args(["torch==2.11.0", "torchaudio==2.11.0", "--index-url", index]);
    run_tool(gen, c, if gpu { "Installing PyTorch (GPU)" } else { "Installing PyTorch" }, expect)?;

    let mut c = Command::new(&uv);
    c.args(["pip", "install", "--python"]).arg(&py).args(["espnet==202511"]);
    run_tool(gen, c, "Installing ESPnet", expect)?;

    // import once: fetches ESPnet's small NLTK data into our own folder and proves the install works
    PROGRESS.store(-1, Relaxed);
    setup_status(gen, "Checking IPA runtime…");
    let mut c = Command::new(&py);
    c.args(["-c", "import torch, espnet2.bin.s2t_inference"]);
    run_tool(gen, c, "Checking IPA runtime", 0)?;

    let _ = std::fs::remove_dir_all(dir.join("uvcache")); // wheels are only needed during install
    std::fs::write(dir.join(IPA_MARK), "").map_err(|e| e.to_string())?;
    Ok(())
}

fn ensure_powsm(gen: u32, cfg: &Config) -> Result<PathBuf, String> {
    let root = cfg.models_dir.join("powsm");
    for (path, sha) in powsm_files() {
        let dest = root.join(&path);
        if dest.exists() {
            continue;
        }
        if !IPA_CONSENT.load(Relaxed) {
            return Err("IPA model missing — pick it again in settings to download".into());
        }
        PROGRESS.store(0, Relaxed);
        let url = format!("https://huggingface.co/espnet/powsm/resolve/{POWSM_REV}/{path}");
        dl(gen, &url, &dest, "Downloading IPA model")?;
        PROGRESS.store(-1, Relaxed);
        if util::sha256_file(&dest).unwrap_or_default() != sha {
            let _ = std::fs::remove_file(&dest);
            return Err("IPA model download was corrupt — click to retry".into());
        }
    }
    Ok(root)
}

/// POWSM wants ISO 639-3 language tokens; map the common 2-letter codes, pass 3-letter ones through,
/// and use <unk> (language-independent) for "auto" and anything unknown.
fn ipa_lang(l: &str) -> String {
    let l = l.trim().to_lowercase();
    let three = match l.as_str() {
        "auto" | "" => "unk",
        "en" => "eng",
        "de" => "deu",
        "fr" => "fra",
        "es" => "spa",
        "it" => "ita",
        "pt" => "por",
        "nl" => "nld",
        "ru" => "rus",
        "ja" => "jpn",
        "zh" => "cmn",
        "ko" => "kor",
        "ar" => "arb",
        "hi" => "hin",
        "tr" => "tur",
        "pl" => "pol",
        "sv" => "swe",
        "da" => "dan",
        "no" => "nob",
        "fi" => "fin",
        "cs" => "ces",
        "el" => "ell",
        "he" => "heb",
        "uk" => "ukr",
        "vi" => "vie",
        "id" => "ind",
        "th" => "tha",
        x if x.len() == 3 => x,
        _ => "unk",
    };
    format!("<{three}>")
}

fn launch_ipa(root: &Path, cfg: &Config, gen: u32) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    let dir = ipa_dir();
    let script = dir.join("ipa_server.py");
    std::fs::write(&script, include_str!("../assets/ipa_server.py")).map_err(|e| e.to_string())?;
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_err(|e| e.to_string())?
        .port();
    PORT.store(port, Relaxed);
    // keep the server's output (import errors, CUDA out-of-memory, ...) for diagnosing start-up failures
    let log = std::fs::File::create(dir.join("server.log")).ok();
    let (out, err) = match log.as_ref().and_then(|f| Some((f.try_clone().ok()?, f.try_clone().ok()?))) {
        Some((a, b)) => (Stdio::from(a), Stdio::from(b)),
        None => (Stdio::null(), Stdio::null()),
    };
    let child = Command::new(ipa_python())
        .arg(&script)
        .arg("--root")
        .arg(root)
        .args(["--port", &port.to_string(), "--lang", &ipa_lang(&cfg.lang)])
        .current_dir(&dir)
        .env("NLTK_DATA", dir.join("nltk_data"))
        .env("CUDA_CACHE_PATH", util::data_dir().join("cudacache"))
        .env("CUDA_CACHE_MAXSIZE", "4294967296")
        .env("PYTHONUTF8", "1")
        .env("PYTHONUNBUFFERED", "1")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("Couldn't start IPA engine: {e}"))?;
    unsafe { AssignProcessToJobObject(job() as _, child.as_raw_handle() as _) };
    *SERVER.lock().unwrap() = Some(child);
    setup_status(gen, "Loading IPA model…");
    wait_ready(port, gen, "IPA engine crashed on start (out of GPU memory? close other GPU apps) — see ipa\\server.log")
}

fn setup_ipa(cfg: &Config, gen: u32) -> Result<(), String> {
    if !ipa_runtime_ready() {
        if !IPA_CONSENT.load(Relaxed) {
            return Err("IPA needs an extra download — pick it again in settings".into());
        }
        install_ipa_runtime(gen)?;
    }
    let root = ensure_powsm(gen, cfg)?;
    // optional text stage (Gemma): engine + model are only fetched with the same consent as the runtime
    let text = ensure_text(gen, cfg)?;
    PROGRESS.store(-1, Relaxed);
    launch_ipa(&root, cfg, gen)?;
    if let Some((exe, model)) = text {
        crate::llm::launch(&exe, &model, gen)?;
    }
    setup_status(gen, "Warming up GPU…");
    transcribe_raw(&vec![0i16; 16000], Duration::from_secs(900))?;
    if crate::llm::mode() != AiMode::Off {
        let _ = crate::llm::decode_ipa("hɛloʊwɜːld"); // first request allocates buffers; do it now
        crate::llm::warm();
    }
    if !stale(gen) {
        READY.store(true, Relaxed);
        setup_status(gen, "Ready (IPA) — click the mic");
        util::refresh();
    }
    Ok(())
}

/// Text AI (Gemma) for any mode except Off: engine + model, fetched only with the user's consent.
fn ensure_text(gen: u32, cfg: &Config) -> Result<Option<(PathBuf, PathBuf)>, String> {
    if cfg.ai_mode == AiMode::Off {
        return Ok(None);
    }
    if !(crate::llm::llama_ready() && crate::llm::text_model_ready(cfg)) && !IPA_CONSENT.load(Relaxed) {
        return Err("Text AI needs a download — pick the AI mode again in settings".into());
    }
    let exe = crate::llm::ensure_llama(gen)?;
    let model = crate::llm::ensure_text_model(gen, cfg)?;
    Ok(Some((exe, model)))
}
