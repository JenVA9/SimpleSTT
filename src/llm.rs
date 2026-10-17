//! Text stage for IPA mode: a small Gemma 4 model (via llama.cpp's llama-server) turns the raw IPA
//! phones into properly written text, in whatever language(s) were spoken. Reasoning is off.
//!
//! Like the POWSM runtime this is only ever fetched after the user picked the IPA model and accepted the
//! warning dialog (engine::IPA_CONSENT). The engine lives in %LOCALAPPDATA%\SimpleSTT\llama, the model in the
//! user's "text models" folder.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::engine::{self, Config, CUSTOM};
use crate::util;

// ---- model catalog ----------------------------------------------------------------------------
pub struct TextModel {
    pub id: &'static str,
    pub label: &'static str,
    pub repo: &'static str,
    pub rev: &'static str, // pinned commit: the file can't change under us
    pub file: &'static str,
    pub mb: u32,
    pub sha: &'static str,
}

/// Gemma 4 (Apache-2.0), QAT-based GGUFs from Unsloth, smallest → largest.
pub const TEXT_MODELS: &[TextModel] = &[
    TextModel {
        id: "gemma4-e2b-q2",
        label: "Gemma 4 E2B · Q2 (tiny)",
        repo: "unsloth/gemma-4-E2B-it-qat-GGUF",
        rev: "66a399f68ddd113b06dff02fca9523e55465d11d",
        file: "gemma-4-E2B-it-qat-UD-Q2_K_XL.gguf",
        mb: 2085,
        sha: "0a5bbc20f91f92da96ab4870fa71b356c45b8500a7b8b9c3e0eb48359b72da28",
    },
    TextModel {
        id: "gemma4-e2b",
        label: "Gemma 4 E2B · Q4",
        repo: "unsloth/gemma-4-E2B-it-qat-GGUF",
        rev: "66a399f68ddd113b06dff02fca9523e55465d11d",
        file: "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf",
        mb: 2499,
        sha: "e531007218dfab990486a5de7676a6932d6ea8dea233d1f698d7c21cf8a16889",
    },
    TextModel {
        id: "gemma4-e4b-q2",
        label: "Gemma 4 E4B · Q2",
        repo: "unsloth/gemma-4-E4B-it-qat-GGUF",
        rev: "8c5a9e4fd5482e2be20fe0bf013b4c262a8f4265",
        file: "gemma-4-E4B-it-qat-UD-Q2_K_XL.gguf",
        mb: 3070,
        sha: "79dde517866cfbb5c00230b530de17910fc7fc78f8827554d0e14281ce5faf03",
    },
    TextModel {
        id: "gemma4-e4b",
        label: "Gemma 4 E4B · Q4",
        repo: "unsloth/gemma-4-E4B-it-qat-GGUF",
        rev: "8c5a9e4fd5482e2be20fe0bf013b4c262a8f4265",
        file: "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf",
        mb: 4021,
        sha: "df0fd4ee07072c607c29a0a1cb4f98918426cca12f45a2776bdd6ee6d09a4de3",
    },
    TextModel {
        id: "gemma4-e4b-google",
        label: "Gemma 4 E4B · Q4_0 (Google official)",
        repo: "google/gemma-4-E4B-it-qat-q4_0-gguf",
        rev: "4b4a2c1d584be7264f87aac328a1bc739ce81b6c",
        file: "gemma-4-E4B_q4_0-it.gguf",
        mb: 4916,
        sha: "676c35070db6dbe52f93e9c864ee0fba4eddea94b9c875d9cb10daff453fbaee",
    },
    TextModel {
        id: "gemma4-12b",
        label: "Gemma 4 12B · Q4",
        repo: "unsloth/gemma-4-12B-it-qat-GGUF",
        rev: "980b060c40a8539ac159e0501a3e0f66a6365af3",
        file: "gemma-4-12B-it-qat-UD-Q4_K_XL.gguf",
        mb: 6405,
        sha: "90fd44e29e0d7cffeb0fd00dc73cfdab9ed0b0e95306ecf7821ea634c940c370",
    },
    TextModel {
        id: "gemma4-26b",
        label: "Gemma 4 26B-A4B · Q4",
        repo: "unsloth/gemma-4-26B-A4B-it-qat-GGUF",
        rev: "7b92b5b28818151e8669af2e45e88d6086f490dd",
        file: "gemma-4-26B-A4B-it-qat-UD-Q4_K_XL.gguf",
        mb: 13589,
        sha: "a7c5bc715f5ff8e99a3e8901ce7d2b42b402c669bf24f7c5250747633d0f5891",
    },
    TextModel {
        id: "gemma4-31b",
        label: "Gemma 4 31B · Q4",
        repo: "unsloth/gemma-4-31B-it-qat-GGUF",
        rev: "43cc1aeb31adf47ec06a854507ce552cd9862e6f",
        file: "gemma-4-31B-it-qat-UD-Q4_K_XL.gguf",
        mb: 16486,
        sha: "00b5a7c497f0c8934033088c10a7fa9a4c015e46ee6d89e9c6890650ba5d0e71",
    },
];

pub fn text_model(id: &str) -> Option<&'static TextModel> {
    TEXT_MODELS.iter().find(|m| m.id == id)
}

// ---- hardware → recommendation -----------------------------------------------------------------
/// Total system RAM in MiB.
pub fn ram_mb() -> u32 {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    unsafe {
        let mut m: MEMORYSTATUSEX = std::mem::zeroed();
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut m) != 0 {
            (m.ullTotalPhys / (1024 * 1024)) as u32
        } else {
            8192
        }
    }
}

/// Dedicated VRAM of the biggest NVIDIA GPU in MiB (via nvidia-smi, which ships with the driver).
pub fn vram_mb() -> Option<u32> {
    static V: OnceLock<Option<u32>> = OnceLock::new();
    *V.get_or_init(|| {
        let out = Command::new("nvidia-smi")
            .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse::<u32>().ok()).max()
    })
}

/// How much memory the text model may use while loaded: half of VRAM (or of RAM without an NVIDIA GPU),
/// but never more than 5 GiB.
pub fn budget_mb() -> u32 {
    (vram_mb().unwrap_or_else(ram_mb) / 2).min(5 * 1024)
}

/// File size + a rough allowance for KV cache and compute buffers.
pub fn est_loaded_mb(file_mb: u32) -> u32 {
    file_mb * 108 / 100 + 350
}

/// Index into TEXT_MODELS: the largest model that fits the budget (smallest if none does).
pub fn recommended_text() -> usize {
    let b = budget_mb();
    TEXT_MODELS.iter().rposition(|m| est_loaded_mb(m.mb) <= b).unwrap_or(0)
}

/// Recommended Whisper model for this machine (first-run wizard, "recommended" tag).
pub fn recommended_whisper() -> &'static str {
    match vram_mb() {
        Some(v) if v >= 6000 => "large-v3-turbo-q5_0",
        Some(v) if v >= 3000 => "small",
        _ if ram_mb() >= 8000 => "small",
        _ => "base",
    }
}

// ---- engine (llama.cpp) --------------------------------------------------------------------------
const LLAMA_REL: &str = "https://github.com/ggml-org/llama.cpp/releases/download/b11703/";
// (variant dir, asset, sha256, size MB). Pinned build.
const VULKAN: (&str, &str, &str, u32) = (
    "vulkan",
    "llama-b11703-bin-win-vulkan-x64.zip",
    "93b7db0f380a0c021007d6df35a48460fbedd27be7f9b76db9238cb5c6f03ffe",
    30,
);
const CPU: (&str, &str, &str, u32) = (
    "cpu",
    "llama-b11703-bin-win-cpu-x64.zip",
    "cf3050ac140fb9d8c726cdea5e3c16b1879c86e41443aabb84c948e9e100b9de",
    18,
);

fn has_vulkan() -> bool {
    use windows_sys::Win32::System::LibraryLoader::LoadLibraryA;
    unsafe { !LoadLibraryA(b"vulkan-1.dll\0".as_ptr()).is_null() }
}

fn variant() -> (&'static str, &'static str, &'static str, u32) {
    if has_vulkan() { VULKAN } else { CPU }
}

pub fn llama_dir() -> PathBuf {
    util::data_dir().join("llama")
}

fn llama_exe() -> PathBuf {
    llama_dir().join(variant().0).join("llama-server.exe")
}

pub fn llama_ready() -> bool {
    llama_exe().exists()
}

pub fn llama_mb() -> u32 {
    variant().3
}

/// Where the selected text model's file is (or will be downloaded to).
pub fn text_model_path(cfg: &Config) -> PathBuf {
    if cfg.text_model == CUSTOM {
        return cfg.custom_text.clone();
    }
    match text_model(&cfg.text_model) {
        Some(m) => cfg.text_dir.join(m.file),
        None => cfg.text_dir.join("missing.gguf"),
    }
}

pub fn text_model_ready(cfg: &Config) -> bool {
    text_model_path(cfg).exists()
}

/// Removes only the model files SimpleSTT itself downloaded (known file names) from `dir`.
pub fn remove_text_models(dir: &Path) {
    for m in TEXT_MODELS {
        let _ = std::fs::remove_file(dir.join(m.file));
        let _ = std::fs::remove_file(dir.join(format!("{}.part", m.file)));
    }
    let _ = std::fs::remove_dir(dir); // only succeeds if now empty
}

pub fn ensure_llama(gen: u32) -> Result<PathBuf, String> {
    let v = variant();
    let exe = llama_exe();
    if exe.exists() {
        return Ok(exe);
    }
    let root = llama_dir();
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let zip = root.join(v.1);
    engine::dl(gen, &format!("{LLAMA_REL}{}", v.1), &zip, "Downloading text engine")?;
    if util::sha256_file(&zip).unwrap_or_default() != v.2 {
        let _ = std::fs::remove_file(&zip);
        return Err("Text engine download was corrupt — click to retry".into());
    }
    let tmp = root.join(format!("{}.tmp", v.0));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    if !engine::unzip(&zip, &tmp) || !tmp.join("llama-server.exe").exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("Couldn't unpack the text engine".into());
    }
    let _ = std::fs::remove_dir_all(root.join(v.0));
    std::fs::rename(&tmp, root.join(v.0)).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(&zip);
    Ok(exe)
}

pub fn ensure_text_model(gen: u32, cfg: &Config) -> Result<PathBuf, String> {
    let path = text_model_path(cfg);
    if path.exists() {
        return Ok(path);
    }
    let Some(m) = text_model(&cfg.text_model) else {
        return Err("Custom text model file not found — pick it again in settings".into());
    };
    if !engine::IPA_CONSENT.load(Relaxed) {
        return Err("Text model missing — pick it again in settings to download".into());
    }
    let url = format!("https://huggingface.co/{}/resolve/{}/{}", m.repo, m.rev, m.file);
    engine::PROGRESS.store(0, Relaxed);
    engine::dl(gen, &url, &path, "Downloading text model")?;
    engine::PROGRESS.store(-1, Relaxed);
    if util::sha256_file(&path).unwrap_or_default() != m.sha {
        let _ = std::fs::remove_file(&path);
        return Err("Text model download was corrupt — click to retry".into());
    }
    Ok(path)
}

// ---- server ---------------------------------------------------------------------------------------
pub static SERVER: Mutex<Option<Child>> = Mutex::new(None);
static PORT: AtomicU16 = AtomicU16::new(0);

pub fn kill_server() {
    if let Ok(mut g) = SERVER.lock() {
        if let Some(mut c) = g.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn health(port: u16) -> Option<u16> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(300)).ok()?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(3)));
    s.write_all(format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").as_bytes()).ok()?;
    let mut b = Vec::new();
    let _ = s.read_to_end(&mut b);
    String::from_utf8_lossy(&b).split_whitespace().nth(1)?.parse().ok()
}

pub fn launch(exe: &Path, model: &Path, gen: u32) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map_err(|e| e.to_string())?
        .port();
    PORT.store(port, Relaxed);
    let gpu_layers = if variant().0 == "cpu" { "0" } else { "999" };
    let child = Command::new(exe)
        .arg("-m")
        .arg(model)
        .args(["--host", "127.0.0.1", "--port", &port.to_string()])
        // small context (a phrase of IPA + a little history), one slot, no web UI, no thinking
        .args(["-c", "4096", "-np", "1", "-ngl", gpu_layers, "-fa", "on", "--reasoning", "off", "--no-ui", "--no-mmproj-auto"])
        .current_dir(exe.parent().unwrap_or(Path::new(".")))
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Couldn't start text engine: {e}"))?;
    unsafe { AssignProcessToJobObject(engine::job() as _, child.as_raw_handle() as _) };
    *SERVER.lock().unwrap() = Some(child);

    engine::setup_status(gen, "Loading text model…");
    let t0 = Instant::now();
    loop {
        if engine::stale(gen) {
            return Err("cancelled".into());
        }
        if let Some(c) = SERVER.lock().unwrap().as_mut() {
            if let Ok(Some(_)) = c.try_wait() {
                return Err("Text engine crashed on start — try a smaller text model".into());
            }
        }
        if health(port) == Some(200) {
            return Ok(());
        }
        if t0.elapsed() > Duration::from_secs(300) {
            return Err("Text engine took too long to start".into());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

// ---- formatting -------------------------------------------------------------------------------------
const SYSTEM_PROMPT: &str = include_str!("../assets/ipa_prompt.txt");
/// Worked examples ("IPA: …" / "OUT: …" line pairs) sent as chat turns: measurably more accurate than the
/// prompt alone on Gemma 4 E4B (0.86 vs 0.75 on the en/de/ja test set).
const EXAMPLES: &str = include_str!("../assets/ipa_examples.txt");

/// Builds the constant part of the request (system prompt + examples) once.
fn prefix() -> &'static str {
    static P: OnceLock<String> = OnceLock::new();
    P.get_or_init(|| {
        let mut m = format!("{{\"role\":\"system\",\"content\":{}}}", util::json_quote(SYSTEM_PROMPT.trim()));
        let mut ipa = "";
        for l in EXAMPLES.lines() {
            if let Some(i) = l.strip_prefix("IPA: ") {
                ipa = i;
            } else if let Some(o) = l.strip_prefix("OUT: ") {
                m.push_str(&format!(
                    ",{{\"role\":\"user\",\"content\":{}}},{{\"role\":\"assistant\",\"content\":{}}}",
                    util::json_quote(&format!("IPA: {ipa}")),
                    util::json_quote(o)
                ));
            }
        }
        m
    })
}

/// Turns one chunk of raw IPA into written text. Each chunk is formatted on its own: feeding earlier text
/// back in made the model drift toward it (it invented continuations), so there is deliberately no context.
pub fn format(ipa: &str) -> Result<String, String> {
    let body = format!(
        "{{\"messages\":[{},{{\"role\":\"user\",\"content\":{}}}],\"temperature\":0.0,\"top_p\":0.95,\"top_k\":64,\"max_tokens\":400,\"stream\":false,\"cache_prompt\":true}}",
        prefix(),
        util::json_quote(&format!("IPA: {ipa}"))
    );
    let resp = engine::http_post(PORT.load(Relaxed), "/v1/chat/completions", "application/json", body.as_bytes(), Duration::from_secs(300))?;
    let text = util::json_str(&resp, "content").ok_or("text engine error")?;
    Ok(text.trim().trim_matches('`').trim().to_string())
}
