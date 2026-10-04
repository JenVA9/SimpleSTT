//! HTTPS via the OS's WinHTTP: zero extra dependencies, uses the system cert store and proxy settings.
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Networking::WinHttp::*;

use crate::util::w;

pub enum Ev<'a> {
    /// Response headers arrived: HTTP status and Content-Length of this response (0 = unknown).
    Head(u32, u64),
    Data(&'a [u8]),
}

struct Handle(*mut c_void);
impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WinHttpCloseHandle(self.0) };
        }
    }
}

/// Streams a GET (optionally with a Range header). The callback returns `false` to abort.
/// Returns the HTTP status code.
pub fn request(url: &str, range_from: u64, cb: &mut dyn FnMut(Ev) -> bool) -> Result<u32, String> {
    let rest = url.strip_prefix("https://").ok_or("only https is supported")?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    unsafe {
        let ses = Handle(WinHttpOpen(
            w(concat!("SimpleSTT/", env!("CARGO_PKG_VERSION"))).as_ptr(),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            null(),
            null(),
            0,
        ));
        if ses.0.is_null() {
            return Err("network init failed".into());
        }
        WinHttpSetTimeouts(ses.0, 15_000, 15_000, 30_000, 60_000);
        let con = Handle(WinHttpConnect(ses.0, w(host).as_ptr(), 443, 0));
        if con.0.is_null() {
            return Err("connect failed".into());
        }
        let req = Handle(WinHttpOpenRequest(
            con.0,
            w("GET").as_ptr(),
            w(path).as_ptr(),
            null(),
            null(),
            null(),
            WINHTTP_FLAG_SECURE,
        ));
        if req.0.is_null() {
            return Err("request failed".into());
        }
        if range_from > 0 {
            let h = w(&format!("Range: bytes={range_from}-"));
            WinHttpAddRequestHeaders(req.0, h.as_ptr(), u32::MAX, WINHTTP_ADDREQ_FLAG_ADD);
        }
        if WinHttpSendRequest(req.0, null(), 0, null(), 0, 0, 0) == 0
            || WinHttpReceiveResponse(req.0, null_mut()) == 0
        {
            return Err("no response (offline?)".into());
        }
        let num = |q: u32| -> u32 {
            let mut v = 0u32;
            let mut len = 4u32;
            WinHttpQueryHeaders(
                req.0,
                q | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                &mut v as *mut u32 as *mut c_void,
                &mut len,
                null_mut(),
            );
            v
        };
        let status = num(WINHTTP_QUERY_STATUS_CODE);
        // Content-Length can exceed 4 GiB in theory; models don't, so 32-bit is fine.
        let clen = num(WINHTTP_QUERY_CONTENT_LENGTH) as u64;
        if !cb(Ev::Head(status, clen)) {
            return Ok(status);
        }
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let mut got = 0u32;
            if WinHttpReadData(req.0, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut got) == 0 {
                return Err("connection lost".into());
            }
            if got == 0 {
                return Ok(status);
            }
            if !cb(Ev::Data(&buf[..got as usize])) {
                return Ok(status);
            }
        }
    }
}

pub fn get_string(url: &str) -> Result<String, String> {
    let mut body = Vec::new();
    let mut status = 0;
    request(url, 0, &mut |e| {
        match e {
            Ev::Head(s, _) => status = s,
            Ev::Data(d) => body.extend_from_slice(d),
        }
        true
    })?;
    if status != 200 {
        return Err(format!("HTTP {status}"));
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn part_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

/// Resumable download to `dest` (via `dest.part`). `progress(done, total)` returns `false` to cancel.
/// `total` is 0 when unknown.
pub fn download(url: &str, dest: &Path, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<(), String> {
    let part = part_path(dest);
    if let Some(p) = dest.parent() {
        std::fs::create_dir_all(p).map_err(|e| format!("can't create folder: {e}"))?;
    }
    let mut last_err = String::new();
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(2));
        }
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        let mut file: Option<File> = None;
        let mut base = 0u64;
        let mut total = 0u64;
        let mut written = 0u64;
        let mut cancelled = false;
        let mut io_err: Option<String> = None;
        let status = request(url, have, &mut |ev| match ev {
            Ev::Head(s, len) => {
                let f = match s {
                    206 => {
                        base = have;
                        OpenOptions::new().append(true).open(&part)
                    }
                    200 => {
                        base = 0;
                        File::create(&part)
                    }
                    _ => return false,
                };
                match f {
                    Ok(f) => file = Some(f),
                    Err(e) => {
                        io_err = Some(format!("can't write file: {e}"));
                        return false;
                    }
                }
                total = if len > 0 { base + len } else { 0 };
                true
            }
            Ev::Data(d) => {
                if let Some(f) = file.as_mut() {
                    if let Err(e) = f.write_all(d) {
                        io_err = Some(format!("can't write file: {e}"));
                        return false;
                    }
                }
                written += d.len() as u64;
                if !progress(base + written, total) {
                    cancelled = true;
                    return false;
                }
                true
            }
        });
        drop(file);
        if cancelled {
            return Err("cancelled".into());
        }
        if let Some(e) = io_err {
            return Err(e);
        }
        match status {
            Ok(200) | Ok(206) if total == 0 || base + written == total => {
                std::fs::rename(&part, dest).map_err(|e| format!("can't finalize file: {e}"))?;
                return Ok(());
            }
            Ok(416) => {
                let _ = std::fs::remove_file(&part); // stale partial; start over
                last_err = "bad resume".into();
            }
            Ok(200) | Ok(206) => last_err = "incomplete download".into(),
            Ok(s) => return Err(format!("server returned HTTP {s}")),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}
