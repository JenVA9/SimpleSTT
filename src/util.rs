//! Small shared helpers: wide strings, paths, SHA-256, clipboard, tiny JSON string scraping.
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{GlobalFree, HWND};
use windows_sys::Win32::Security::Cryptography::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

pub const WM_REFRESH: u32 = WM_APP + 1;
pub const WM_RESULT: u32 = WM_APP + 2;

pub static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);

pub fn main_hwnd() -> HWND {
    MAIN_HWND.load(Ordering::Relaxed) as HWND
}

/// Ask the UI thread to repaint / re-evaluate animation. Safe from any thread.
pub fn refresh() {
    let h = main_hwnd();
    if !h.is_null() {
        unsafe { PostMessageW(h, WM_REFRESH, 0, 0) };
    }
}

pub fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn from_wide(b: &[u16]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf16_lossy(&b[..n])
}

/// Everything SimpleSTT installs lives here (no admin needed): %LOCALAPPDATA%\SimpleSTT
/// Where dev mode collects its examples (one file, only ever cleared by the user).
pub fn feedback_file() -> PathBuf {
    data_dir().join("dev-feedback.txt")
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("SimpleSTT")
}

pub fn sha256_file(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    unsafe {
        let mut alg = null_mut();
        if BCryptOpenAlgorithmProvider(&mut alg, w("SHA256").as_ptr(), null(), 0) != 0 {
            return None;
        }
        let mut hash = null_mut();
        if BCryptCreateHash(alg, &mut hash, null_mut(), 0, null(), 0, 0) != 0 {
            BCryptCloseAlgorithmProvider(alg, 0);
            return None;
        }
        let mut buf = vec![0u8; 1 << 20];
        let mut ok = true;
        loop {
            match f.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if BCryptHashData(hash, buf.as_ptr(), n as u32, 0) != 0 {
                        ok = false;
                        break;
                    }
                }
                Err(_) => {
                    ok = false;
                    break;
                }
            }
        }
        let mut out = [0u8; 32];
        ok = ok && BCryptFinishHash(hash, out.as_mut_ptr(), 32, 0) == 0;
        BCryptDestroyHash(hash);
        BCryptCloseAlgorithmProvider(alg, 0);
        if !ok {
            return None;
        }
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(64);
        for b in out {
            s.push(HEX[(b >> 4) as usize] as char);
            s.push(HEX[(b & 15) as usize] as char);
        }
        Some(s)
    }
}

/// Must be called on the UI thread (the clipboard owner window lives there).
pub fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    let data: Vec<u16> = w(text);
    unsafe {
        let mut opened = false;
        for _ in 0..20 {
            if OpenClipboard(hwnd) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if !opened {
            return false;
        }
        EmptyClipboard();
        let bytes = data.len() * 2;
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes);
        let mut ok = false;
        if !h.is_null() {
            let p = GlobalLock(h) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
                GlobalUnlock(h);
                // 13 = CF_UNICODETEXT. On success the clipboard owns `h`.
                ok = !SetClipboardData(13, h as *mut c_void).is_null();
            }
            if !ok {
                GlobalFree(h);
            }
        }
        CloseClipboard();
        ok
    }
}

/// Find `"key"` then return the JSON string value that follows (unescaped).
/// Intentionally tiny: used for GitHub release metadata and whisper-server replies.
pub fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let mut i = s.find(&pat)? + pat.len();
    let b = s.as_bytes();
    while i < b.len() && (b[i] == b' ' || b[i] == b':' || b[i] == b'\n' || b[i] == b'\r' || b[i] == b'\t') {
        i += 1;
    }
    if i >= b.len() || b[i] != b'"' {
        return None;
    }
    let mut out = String::new();
    let mut it = s[i + 1..].chars();
    while let Some(c) = it.next() {
        match c {
            '"' => return Some(out),
            '\\' => match it.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' | 'f' => {}
                'u' => {
                    let hex = |it: &mut std::str::Chars| -> Option<u32> {
                        let h: String = it.by_ref().take(4).collect();
                        u32::from_str_radix(&h, 16).ok()
                    };
                    let mut cp = hex(&mut it)?;
                    if (0xD800..0xDC00).contains(&cp) {
                        // surrogate pair: expect \uXXXX
                        if it.next()? == '\\' && it.next()? == 'u' {
                            let lo = hex(&mut it)?;
                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF);
                        }
                    }
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
                other => out.push(other), // \" \\ \/
            },
            c => out.push(c),
        }
    }
    None
}


/// A JSON string literal (with quotes) for `s`.
pub fn json_quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// Set SIMPLESTT_TEST_ID to run a second, isolated instance (own single-instance lock and window classes),
/// e.g. for automated tests next to a running copy. Empty in normal use.
pub fn test_id() -> String {
    std::env::var("SIMPLESTT_TEST_ID").unwrap_or_default()
}

/// Window class name for this instance.
pub fn class(base: &str) -> String {
    format!("{base}{}", test_id())
}
