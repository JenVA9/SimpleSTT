//! Self-update from GitHub Releases. No installer, no admin: Windows lets a running .exe be renamed,
//! so we download the new build next to it, rename old -> .old, new -> current, and relaunch.
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

use crate::{engine, net, util};

const REPO: &str = "JenVA9/SimpleSTT";
const ASSET: &str = "SimpleSTT.exe";

fn parse(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next().flatten().unwrap_or(0), it.next().flatten().unwrap_or(0)))
}

pub fn spawn() {
    std::thread::spawn(|| {
        let _ = run();
    });
}

fn run() -> Option<()> {
    let exe = std::env::current_exe().ok()?;
    let old = exe.with_extension("exe.old");
    let new = exe.with_extension("exe.new");
    let _ = std::fs::remove_file(&old); // leftover from the previous update
    let _ = std::fs::remove_file(&new);

    std::thread::sleep(Duration::from_secs(2)); // don't compete with startup
    let json = net::get_string(&format!("https://api.github.com/repos/{REPO}/releases/latest")).ok()?;
    let tag = util::json_str(&json, "tag_name")?;
    if parse(&tag)? <= parse(env!("CARGO_PKG_VERSION"))? {
        return None;
    }
    let from = &json[json.find(&format!("\"{ASSET}\""))?..];
    let url = util::json_str(from, "browser_download_url")?;
    let digest = util::json_str(from, "digest")?;
    let want = digest.strip_prefix("sha256:")?.to_ascii_lowercase();

    net::download(&url, &new, &mut |_, _| true).ok()?;
    if util::sha256_file(&new).as_deref() != Some(want.as_str()) {
        let _ = std::fs::remove_file(&new);
        return None;
    }
    // Never yank the app away mid-dictation.
    while matches!(engine::STATE.load(Relaxed), engine::REC | engine::TRANS) {
        std::thread::sleep(Duration::from_secs(1));
    }
    swap(&exe, &old, &new)
}

fn swap(exe: &PathBuf, old: &PathBuf, new: &PathBuf) -> Option<()> {
    std::fs::rename(exe, old).ok()?;
    if std::fs::rename(new, exe).is_err() {
        let _ = std::fs::rename(old, exe);
        return None;
    }
    std::process::Command::new(exe).arg("--relaunch").spawn().ok()?;
    let h = util::main_hwnd();
    if !h.is_null() {
        unsafe { PostMessageW(h, WM_CLOSE, 0, 0) };
    }
    Some(())
}
