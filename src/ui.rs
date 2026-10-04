//! The whole UI: a tiny custom-drawn window (mic button + settings gear) and a native settings window.
//! Raw Win32 + GDI+ (anti-aliased shapes); no UI framework, so the binary stays tiny.
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU8, Ordering::*};
use std::sync::{Mutex, Once, OnceLock, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Graphics::GdiPlus::*;
use windows_sys::Win32::System::Com::{CoInitializeEx, CoTaskMemFree};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use std::result::Result::{Err, Ok}; // GDI+ also exports an `Ok` status constant; keep Result's.

use crate::engine::{self, Config, DONE, IDLE, MODELS, REC, STATE, TRANS};
use crate::util::{self, WM_REFRESH, WM_RESULT, from_wide, w};
use crate::{audio, update};

// ---- layout (96-dpi units) ---------------------------------------------------------------------
const W: i32 = 220;
const H: i32 = 160;
const CX: f32 = 110.0;
const CY: f32 = 62.0;
const R: f32 = 40.0;
const GX: f32 = 196.0;
const GY: f32 = 22.0;

const WM_MOUSELEAVE: u32 = 0x02A3;

const BG: u32 = rgb(0x1b, 0x1b, 0x1f);
const GRAY: u32 = rgb(0x3a, 0x3a, 0x44);
const GRAY_H: u32 = rgb(0x4a, 0x4a, 0x57);
const RED: u32 = rgb(0xe5, 0x48, 0x4d);
const AMBER: u32 = rgb(0xf5, 0xa5, 0x24);
const GREEN: u32 = rgb(0x30, 0xa4, 0x6c);

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}
fn argb(c: u32, a: u32) -> u32 {
    (a << 24) | ((c & 0xff) << 16) | (c & 0xff00) | ((c >> 16) & 0xff)
}

static DPI: AtomicU32 = AtomicU32::new(96);
static HOVER: AtomicU8 = AtomicU8::new(0);
static TRACKING: AtomicBool = AtomicBool::new(false);
static TIMER_ON: AtomicBool = AtomicBool::new(false);
static SETTINGS: AtomicIsize = AtomicIsize::new(0);
static F_TEXT: AtomicIsize = AtomicIsize::new(0);
static F_MIC: AtomicIsize = AtomicIsize::new(0);
static F_GEAR: AtomicIsize = AtomicIsize::new(0);
static T0: OnceLock<Instant> = OnceLock::new();
static RESULT: Mutex<Option<String>> = Mutex::new(None);
static CFG: Mutex<Option<Config>> = Mutex::new(None);
static REC_STOP: AtomicBool = AtomicBool::new(false);
static REC_JOB: Mutex<Option<JoinHandle<Vec<i16>>>> = Mutex::new(None);

fn s(x: i32) -> i32 {
    x * DPI.load(Relaxed) as i32 / 96
}
fn sf(x: f32) -> f32 {
    x * DPI.load(Relaxed) as f32 / 96.0
}
fn cfg() -> Config {
    CFG.lock().unwrap().clone().unwrap()
}

// ---- entry ------------------------------------------------------------------------------------
pub fn run() {
    unsafe {
        SetProcessDPIAware();
        let dc = GetDC(null_mut());
        DPI.store(GetDeviceCaps(dc, LOGPIXELSX as i32) as u32, Relaxed);
        ReleaseDC(null_mut(), dc);
        CoInitializeEx(null(), 2 /* apartment threaded */);
        let mut token = 0usize;
        let gi = GdiplusStartupInput {
            GdiplusVersion: 1,
            DebugEventCallback: 0,
            SuppressBackgroundThread: 0,
            SuppressExternalCodecs: 0,
        };
        GdiplusStartup(&mut token, &gi, null_mut());
        T0.get_or_init(Instant::now);

        F_TEXT.store(make_font("Segoe UI", 13, 400) as isize, Relaxed);
        let icons = if font_exists("Segoe Fluent Icons") { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" };
        F_MIC.store(make_font(icons, 34, 400) as isize, Relaxed);
        F_GEAR.store(make_font(icons, 17, 400) as isize, Relaxed);

        *CFG.lock().unwrap() = Some(Config::load());

        let hinst = GetModuleHandleW(null());
        let cls = w("SimpleSTTWnd");
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(main_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: null_mut(),
            lpszMenuName: null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT { left: 0, top: 0, right: s(W), bottom: s(H) };
        AdjustWindowRectEx(&mut r, style, 0, WS_EX_TOPMOST);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            cls.as_ptr(),
            w("SimpleSTT").as_ptr(),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            r.right - r.left,
            r.bottom - r.top,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        util::MAIN_HWND.store(hwnd as isize, Relaxed);
        let dark = 1i32;
        DwmSetWindowAttribute(hwnd, 20, &dark as *const i32 as *const c_void, 4);
        ShowWindow(hwnd, SW_SHOW);

        engine::start_setup(cfg());
        update::spawn();

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            let sh = SETTINGS.load(Relaxed) as HWND;
            if !sh.is_null() && IsDialogMessageW(sh, &msg) != 0 {
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        engine::kill_server();
    }
}

unsafe fn make_font(face: &str, px: i32, weight: i32) -> HFONT {
    CreateFontW(-s(px), 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, 5, 0, w(face).as_ptr())
}

unsafe fn font_exists(face: &str) -> bool {
    let dc = GetDC(null_mut());
    let f = make_font(face, 12, 400);
    let old = SelectObject(dc, f);
    let mut buf = [0u16; 64];
    GetTextFaceW(dc, 64, buf.as_mut_ptr());
    SelectObject(dc, old);
    DeleteObject(f);
    ReleaseDC(null_mut(), dc);
    from_wide(&buf).eq_ignore_ascii_case(face)
}

// ---- recording flow ---------------------------------------------------------------------------
fn idle(msg: &str, bad: bool) {
    STATE.store(IDLE, Relaxed);
    engine::set_status(msg, bad);
}

fn toggle_mic() {
    match STATE.load(Relaxed) {
        REC => stop_rec(),
        TRANS => {}
        _ => {
            if engine::FAILED.load(Relaxed) {
                engine::set_status("Retrying…", false);
                engine::start_setup(cfg());
            } else {
                start_rec();
            }
        }
    }
}

fn start_rec() {
    REC_STOP.store(false, Release);
    let (tx, rx) = mpsc::channel();
    let h = std::thread::spawn(move || {
        audio::record(&REC_STOP, &|r| {
            let _ = tx.send(r);
        })
    });
    match rx.recv() {
        Ok(Ok(())) => {
            *REC_JOB.lock().unwrap() = Some(h);
            STATE.store(REC, Relaxed);
            engine::set_status("Listening… click to finish", false);
        }
        Ok(Err(e)) => {
            let _ = h.join();
            idle(&e, true);
        }
        Err(_) => idle("Couldn't open the microphone", true),
    }
}

fn stop_rec() {
    REC_STOP.store(true, Release);
    STATE.store(TRANS, Relaxed);
    engine::set_status("Transcribing…", false);
    let job = REC_JOB.lock().unwrap().take();
    std::thread::spawn(move || {
        let samples = job.map(|h| h.join().unwrap_or_default()).unwrap_or_default();
        finish(samples);
    });
}

fn finish(samples: Vec<i16>) {
    if samples.len() < 4800 {
        return idle("Too short — try again", true);
    }
    let mut announced = false;
    while !engine::READY.load(Relaxed) {
        if engine::FAILED.load(Relaxed) {
            STATE.store(IDLE, Relaxed);
            util::refresh();
            return; // setup error text is already on screen
        }
        if !announced {
            announced = true;
            engine::set_status("Waiting for model…", false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    match engine::transcribe(&samples) {
        Ok(t) if !t.is_empty() => {
            *RESULT.lock().unwrap() = Some(t);
            unsafe { PostMessageW(util::main_hwnd(), WM_RESULT, 0, 0) };
        }
        Ok(_) => idle("Didn't catch anything", true),
        Err(e) if e == "engine not running" => {
            STATE.store(IDLE, Relaxed);
            engine::set_status("Engine stopped — restarting…", true);
            engine::start_setup(cfg());
        }
        Err(e) => idle(&e, true),
    }
}

// ---- drawing ----------------------------------------------------------------------------------
fn hit(x: i32, y: i32) -> u8 {
    let (x, y) = (x as f32, y as f32);
    let d = |cx: f32, cy: f32| (x - sf(cx)).powi(2) + (y - sf(cy)).powi(2);
    if d(CX, CY) <= sf(R).powi(2) {
        1
    } else if d(GX, GY) <= sf(16.0).powi(2) {
        2
    } else {
        0
    }
}

unsafe fn fill_circle(g: *mut GpGraphics, cx: f32, cy: f32, r: f32, c: u32) {
    let mut b = null_mut();
    GdipCreateSolidFill(c, &mut b);
    GdipFillEllipse(g, b as *mut GpBrush, cx - r, cy - r, 2.0 * r, 2.0 * r);
    GdipDeleteBrush(b as *mut GpBrush);
}

unsafe fn arc(g: *mut GpGraphics, cx: f32, cy: f32, r: f32, width: f32, c: u32, start: f32, sweep: f32) {
    let mut p = null_mut();
    GdipCreatePen1(c, width, 2, &mut p);
    GdipSetPenStartCap(p, 2);
    GdipSetPenEndCap(p, 2);
    if sweep >= 360.0 {
        GdipDrawEllipse(g, p, cx - r, cy - r, 2.0 * r, 2.0 * r);
    } else {
        GdipDrawArc(g, p, cx - r, cy - r, 2.0 * r, 2.0 * r, start, sweep);
    }
    GdipDeletePen(p);
}

unsafe fn glyph(dc: HDC, font: isize, ch: char, cx: f32, cy: f32, half: f32, color: u32) {
    let old = SelectObject(dc, font as HFONT);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, color);
    let mut r = RECT {
        left: (cx - half) as i32,
        top: (cy - half) as i32,
        right: (cx + half) as i32,
        bottom: (cy + half) as i32,
    };
    let t = [ch as u16, 0];
    DrawTextW(dc, t.as_ptr(), 1, &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(dc, old);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = std::mem::zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc: RECT = std::mem::zeroed();
    GetClientRect(hwnd, &mut rc);
    let mem = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
    let old = SelectObject(mem, bmp);
    let br = CreateSolidBrush(BG);
    FillRect(mem, &rc, br);
    DeleteObject(br);

    let t = T0.get().map(|t| t.elapsed().as_secs_f32()).unwrap_or(0.0);
    let state = STATE.load(Relaxed);
    let busy_setup = state == IDLE && !engine::READY.load(Relaxed) && !engine::FAILED.load(Relaxed);
    let hover = HOVER.load(Relaxed);
    let (col, icon) = match state {
        REC => (RED, '\u{E720}'),
        TRANS => (AMBER, '\u{E720}'),
        DONE => (GREEN, '\u{E73E}'), // check mark
        _ => (if hover == 1 { GRAY_H } else { GRAY }, '\u{E720}'),
    };
    let (cx, cy, r) = (sf(CX), sf(CY), sf(R));

    let mut g = null_mut();
    GdipCreateFromHDC(mem, &mut g);
    GdipSetSmoothingMode(g, 4);
    if state == REC {
        let p = (t * 1.1) % 1.0;
        arc(g, cx, cy, r + sf(4.0) + p * sf(14.0), sf(3.0), argb(RED, ((1.0 - p) * 120.0) as u32), 0.0, 360.0);
    }
    fill_circle(g, cx, cy, r, argb(col, 255));
    if state == TRANS || busy_setup {
        let ring = r + sf(8.0);
        let prog = engine::PROGRESS.load(Relaxed);
        let c = if state == TRANS { AMBER } else { rgb(0x7a, 0xa2, 0xff) };
        if busy_setup && prog >= 0 {
            arc(g, cx, cy, ring, sf(3.0), argb(c, 50), 0.0, 360.0);
            arc(g, cx, cy, ring, sf(3.0), argb(c, 255), -90.0, 360.0 * prog as f32 / 1000.0);
        } else {
            arc(g, cx, cy, ring, sf(3.0), argb(c, 255), (t * 420.0) % 360.0, 90.0);
        }
    }
    GdipDeleteGraphics(g);

    glyph(mem, F_MIC.load(Relaxed), icon, cx, cy, r, rgb(255, 255, 255));
    let gc = if hover == 2 { rgb(255, 255, 255) } else { rgb(0x9a, 0x9a, 0xa5) };
    glyph(mem, F_GEAR.load(Relaxed), '\u{E713}', sf(GX), sf(GY), sf(16.0), gc);

    // status line
    let old_f = SelectObject(mem, F_TEXT.load(Relaxed) as HFONT);
    SetBkMode(mem, TRANSPARENT as i32);
    SetTextColor(mem, if engine::STATUS_BAD.load(Relaxed) { rgb(0xff, 0x8a, 0x8a) } else { rgb(0x9a, 0x9a, 0xa5) });
    let mut tr = RECT { left: s(10), top: s(114), right: rc.right - s(10), bottom: rc.bottom - s(4) };
    let txt = w(&engine::status());
    DrawTextW(mem, txt.as_ptr(), -1, &mut tr, DT_CENTER | DT_WORDBREAK | DT_NOPREFIX);
    SelectObject(mem, old_f);

    BitBlt(hdc, 0, 0, rc.right, rc.bottom, mem, 0, 0, SRCCOPY);
    SelectObject(mem, old);
    DeleteObject(bmp);
    DeleteDC(mem);
    EndPaint(hwnd, &ps);
}

fn lo(l: isize) -> i32 {
    (l & 0xffff) as i16 as i32
}
fn hi(l: isize) -> i32 {
    ((l >> 16) & 0xffff) as i16 as i32
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(hwnd);
            0
        }
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => {
            let h = hit(lo(lp), hi(lp));
            if !TRACKING.swap(true, Relaxed) {
                let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                TrackMouseEvent(&mut t);
            }
            if HOVER.swap(h, Relaxed) != h {
                InvalidateRect(hwnd, null(), 0);
            }
            0
        }
        WM_MOUSELEAVE => {
            TRACKING.store(false, Relaxed);
            HOVER.store(0, Relaxed);
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_SETCURSOR => {
            if (lp & 0xffff) as u32 == HTCLIENT && HOVER.load(Relaxed) != 0 {
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                return 1;
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
        WM_LBUTTONUP => {
            match hit(lo(lp), hi(lp)) {
                1 => toggle_mic(),
                2 => open_settings(hwnd),
                _ => {}
            }
            0
        }
        WM_KEYDOWN => {
            if wp as u16 == VK_SPACE || wp as u16 == VK_RETURN {
                toggle_mic();
            }
            0
        }
        WM_TIMER => {
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_REFRESH => {
            let st = STATE.load(Relaxed);
            let anim = st == REC || st == TRANS || (st == IDLE && !engine::READY.load(Relaxed) && !engine::FAILED.load(Relaxed));
            if anim != TIMER_ON.swap(anim, Relaxed) {
                if anim {
                    SetTimer(hwnd, 1, 33, None);
                } else {
                    KillTimer(hwnd, 1);
                }
            }
            InvalidateRect(hwnd, null(), 0);
            0
        }
        WM_RESULT => {
            if let Some(t) = RESULT.lock().unwrap().take() {
                if util::set_clipboard(hwnd, &t) {
                    STATE.store(DONE, Relaxed);
                    engine::set_status("Copied to clipboard", false);
                } else {
                    idle("Couldn't access the clipboard", true);
                }
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            engine::kill_server();
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- settings window --------------------------------------------------------------------------
const ID_DIR: i32 = 101;
const ID_BROWSE: i32 = 102;
const ID_MODEL: i32 = 103;
const ID_LANG: i32 = 104;
const ID_DELETE: i32 = 107;

unsafe fn open_settings(parent: HWND) {
    let existing = SETTINGS.load(Relaxed) as HWND;
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return;
    }
    static REG: Once = Once::new();
    let hinst = GetModuleHandleW(null());
    let cls = w("SimpleSTTSettings");
    REG.call_once(|| {
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(settings_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);
    });
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut r = RECT { left: 0, top: 0, right: s(460), bottom: s(300) };
    AdjustWindowRectEx(&mut r, style, 0, 0);
    let (ww, wh) = (r.right - r.left, r.bottom - r.top);
    let mut pr: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut pr);
    let x = (pr.left + pr.right - ww) / 2;
    let y = (pr.top + pr.bottom - wh) / 2;
    let h = CreateWindowExW(0, cls.as_ptr(), w("SimpleSTT settings").as_ptr(), style, x, y, ww, wh, parent, null_mut(), hinst, null());
    SETTINGS.store(h as isize, Relaxed);
    EnableWindow(parent, 0);
    ShowWindow(h, SW_SHOW);
}

unsafe fn ctl(parent: HWND, class: &str, text: &str, style: u32, x: i32, y: i32, wd: i32, ht: i32, id: i32) -> HWND {
    let h = CreateWindowExW(
        0,
        w(class).as_ptr(),
        w(text).as_ptr(),
        WS_CHILD | WS_VISIBLE | style,
        s(x),
        s(y),
        s(wd),
        s(ht),
        parent,
        id as isize as HMENU,
        GetModuleHandleW(null()),
        null(),
    );
    SendMessageW(h, WM_SETFONT, F_UI.load(Relaxed) as usize, 1);
    h
}
static F_UI: AtomicIsize = AtomicIsize::new(0);

unsafe fn get_text(h: HWND) -> String {
    let mut b = [0u16; 1024];
    GetWindowTextW(h, b.as_mut_ptr(), 1024);
    from_wide(&b)
}

unsafe fn fill_combo(cb: HWND, dir: &Path, sel: &str) {
    SendMessageW(cb, CB_RESETCONTENT, 0, 0);
    for (i, (n, mb)) in MODELS.iter().enumerate() {
        let size = if *mb >= 1000 { format!("{:.1} GB", *mb as f32 / 1024.0) } else { format!("{mb} MB") };
        let have = if engine::model_path_in(dir, n).exists() { "   ✓ downloaded" } else { "" };
        SendMessageW(cb, CB_ADDSTRING, 0, w(&format!("{n}  —  {size}{have}")).as_ptr() as isize);
        if *n == sel {
            SendMessageW(cb, CB_SETCURSEL, i, 0);
        }
    }
}

unsafe fn browse(owner: HWND) -> Option<PathBuf> {
    let mut bi: BROWSEINFOW = std::mem::zeroed();
    bi.hwndOwner = owner;
    let title = w("Choose where to store Whisper models");
    bi.lpszTitle = title.as_ptr();
    bi.ulFlags = BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE;
    let pidl = SHBrowseForFolderW(&bi);
    if pidl.is_null() {
        return None;
    }
    let mut b = [0u16; 520];
    let ok = SHGetPathFromIDListW(pidl, b.as_mut_ptr()) != 0;
    CoTaskMemFree(pidl as *const c_void);
    ok.then(|| PathBuf::from(from_wide(&b)))
}

/// Removes everything SimpleSTT put on disk (engine, models, settings, caches).
fn wipe(models_dir: &Path) {
    engine::cancel();
    std::thread::sleep(Duration::from_millis(400));
    let dd = util::data_dir();
    if !models_dir.starts_with(&dd) {
        // custom folder: delete only the files we created there, never the user's other files
        if let Ok(rd) = std::fs::read_dir(models_dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("ggml-") && (n.ends_with(".bin") || n.ends_with(".bin.part")) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(models_dir);
    }
    for _ in 0..10 {
        if std::fs::remove_dir_all(&dd).is_ok() || !dd.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(exe.with_extension("exe.old"));
        let _ = std::fs::remove_file(exe.with_extension("exe.new"));
    }
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let f = make_font("Segoe UI", 12, 400);
            F_UI.store(f as isize, Relaxed);
            let c = cfg();
            const EDIT: u32 = WS_TABSTOP | WS_BORDER | 0x80; // ES_AUTOHSCROLL
            const BTN: u32 = WS_TABSTOP;
            ctl(hwnd, "STATIC", "Models folder", 0, 16, 14, 428, 18, 0);
            ctl(hwnd, "EDIT", &c.models_dir.to_string_lossy(), EDIT, 16, 34, 340, 24, ID_DIR);
            ctl(hwnd, "BUTTON", "Browse…", BTN, 364, 33, 80, 26, ID_BROWSE);
            ctl(hwnd, "STATIC", "Model  (downloaded automatically when selected)", 0, 16, 74, 428, 18, 0);
            let cb = ctl(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | 3 /* CBS_DROPDOWNLIST */, 16, 94, 428, 300, ID_MODEL);
            fill_combo(cb, &c.models_dir, &c.model);
            ctl(hwnd, "STATIC", "Language  (auto, en, de, fr, es, … — .en models always use English)", 0, 16, 134, 428, 18, 0);
            ctl(hwnd, "EDIT", &c.lang, EDIT, 16, 154, 120, 24, ID_LANG);
            let dd = format!("Engine, cache and settings live in:\n{}", util::data_dir().display());
            ctl(hwnd, "STATIC", &dd, 0, 16, 196, 428, 36, 0);
            ctl(hwnd, "BUTTON", "Delete all data…", BTN, 16, 252, 130, 30, ID_DELETE);
            ctl(hwnd, "BUTTON", "Save", BTN | 1 /* BS_DEFPUSHBUTTON */, 274, 252, 80, 30, 1);
            ctl(hwnd, "BUTTON", "Cancel", BTN, 364, 252, 80, 30, 2);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xffff) as i32;
            let code = ((wp >> 16) & 0xffff) as u32;
            let dir_h = GetDlgItem(hwnd, ID_DIR);
            let cb = GetDlgItem(hwnd, ID_MODEL);
            match id {
                ID_BROWSE => {
                    if let Some(p) = browse(hwnd) {
                        SetWindowTextW(dir_h, w(&p.to_string_lossy()).as_ptr());
                        let i = SendMessageW(cb, CB_GETCURSEL, 0, 0).max(0) as usize;
                        fill_combo(cb, &p, MODELS[i.min(MODELS.len() - 1)].0);
                    }
                }
                ID_DIR if code == EN_KILLFOCUS as u32 => {
                    let i = SendMessageW(cb, CB_GETCURSEL, 0, 0).max(0) as usize;
                    fill_combo(cb, Path::new(&get_text(dir_h)), MODELS[i.min(MODELS.len() - 1)].0);
                }
                1 => {
                    let old = cfg();
                    let dir = get_text(dir_h);
                    let lang = get_text(GetDlgItem(hwnd, ID_LANG)).trim().to_lowercase();
                    let i = SendMessageW(cb, CB_GETCURSEL, 0, 0).max(0) as usize;
                    let new = Config {
                        models_dir: if dir.trim().is_empty() { old.models_dir.clone() } else { PathBuf::from(dir.trim()) },
                        model: MODELS[i.min(MODELS.len() - 1)].0.to_string(),
                        lang: if lang.is_empty() { "auto".into() } else { lang },
                    };
                    if new != old {
                        new.save();
                        *CFG.lock().unwrap() = Some(new.clone());
                        if STATE.load(Relaxed) == IDLE {
                            engine::start_setup(new);
                        } else {
                            // mid-dictation: apply once the user is done
                            std::thread::spawn(move || {
                                while STATE.load(Relaxed) == REC || STATE.load(Relaxed) == TRANS {
                                    std::thread::sleep(Duration::from_millis(300));
                                }
                                engine::start_setup(new);
                            });
                        }
                    }
                    DestroyWindow(hwnd);
                }
                2 => {
                    DestroyWindow(hwnd);
                }
                ID_DELETE => {
                    let text = w("This deletes the speech engine, every downloaded model, and all settings, then closes SimpleSTT.\n\nSimpleSTT.exe itself is not touched — delete it yourself afterwards.\n\nContinue?");
                    let ok = MessageBoxW(hwnd, text.as_ptr(), w("Delete all SimpleSTT data").as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2);
                    if ok == IDYES {
                        let dir = cfg().models_dir;
                        wipe(&dir);
                        let main = util::main_hwnd();
                        DestroyWindow(hwnd);
                        DestroyWindow(main);
                    }
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            SETTINGS.store(0, Relaxed);
            DeleteObject(F_UI.load(Relaxed) as HFONT);
            let main = util::main_hwnd();
            if IsWindow(main) != 0 {
                EnableWindow(main, 1);
                SetForegroundWindow(main);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
