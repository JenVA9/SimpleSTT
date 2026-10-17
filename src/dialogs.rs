//! Settings window and the first-run wizard (plain Win32 controls), plus the "extra download" warning and
//! the delete-everything routine.
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicIsize, Ordering::Relaxed};
use std::sync::{Mutex, Once};
use std::time::Duration;

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::engine::{self, CUSTOM, Config, MODELS};
use crate::llm::{self, AiMode, TEXT_MODELS};
use crate::ui::{apply_config, cfg, make_font, s};
use crate::util::{self, from_wide, w};

// ---- shared state / helpers -----------------------------------------------------------------------
static SETTINGS: AtomicIsize = AtomicIsize::new(0);
static WIZARD: AtomicIsize = AtomicIsize::new(0);
static F_UI: AtomicIsize = AtomicIsize::new(0);
/// the values being edited in whichever dialog is open
static DRAFT: Mutex<Option<Config>> = Mutex::new(None);
/// last non-"custom" selection of each combo, so cancelling the file picker can restore it
static PREV_SEL: Mutex<[usize; 2]> = Mutex::new([0, 0]);

/// The dialog (settings or wizard) that should get keyboard navigation (Tab / Enter / Esc).
pub fn active_dialog() -> HWND {
    let a = SETTINGS.load(Relaxed) as HWND;
    if !a.is_null() { a } else { WIZARD.load(Relaxed) as HWND }
}

fn draft() -> Config {
    DRAFT.lock().unwrap().clone().unwrap_or_else(cfg)
}
fn set_draft(f: impl FnOnce(&mut Config)) {
    if let Some(d) = DRAFT.lock().unwrap().as_mut() {
        f(d);
    }
}

const EDIT: u32 = WS_TABSTOP | WS_BORDER | 0x80; // ES_AUTOHSCROLL
const BTN: u32 = WS_TABSTOP;

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

unsafe fn get_text(h: HWND) -> String {
    let mut b = [0u16; 1024];
    GetWindowTextW(h, b.as_mut_ptr(), 1024);
    from_wide(&b)
}

fn fmt_mb(mb: u32) -> String {
    if mb >= 1000 { format!("{:.1} GB", mb as f32 / 1024.0) } else { format!("{mb} MB") }
}

unsafe fn browse_folder(owner: HWND, title: &str) -> Option<PathBuf> {
    let mut bi: BROWSEINFOW = std::mem::zeroed();
    bi.hwndOwner = owner;
    let t = w(title);
    bi.lpszTitle = t.as_ptr();
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

unsafe fn pick_file(owner: HWND, title: &str, filter: &str) -> Option<PathBuf> {
    let mut buf = [0u16; 1024];
    let t = w(title);
    // "description\0*.ext\0\0"
    let f: Vec<u16> = filter.encode_utf16().chain([0, 0]).collect();
    let mut o: OPENFILENAMEW = std::mem::zeroed();
    o.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    o.hwndOwner = owner;
    o.lpstrFilter = f.as_ptr();
    o.lpstrFile = buf.as_mut_ptr();
    o.nMaxFile = buf.len() as u32;
    o.lpstrTitle = t.as_ptr();
    o.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_EXPLORER | OFN_NOCHANGEDIR;
    (GetOpenFileNameW(&mut o) != 0).then(|| PathBuf::from(from_wide(&buf)))
}

// ---- model lists ------------------------------------------------------------------------------------
fn whisper_ids() -> Vec<&'static str> {
    MODELS.iter().map(|m| m.0).chain([CUSTOM]).collect()
}
fn text_ids() -> Vec<&'static str> {
    TEXT_MODELS.iter().map(|m| m.id).chain([CUSTOM]).collect()
}

fn file_label(p: &Path) -> String {
    p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
}

unsafe fn fill_whisper(cb: HWND, d: &Config) {
    SendMessageW(cb, CB_RESETCONTENT, 0, 0);
    let rec = llm::recommended_whisper();
    for (i, id) in whisper_ids().iter().enumerate() {
        let label = if *id == CUSTOM {
            if d.custom_model.as_os_str().is_empty() {
                "Custom model file (ggml .bin)…".to_string()
            } else {
                format!("Custom: {}", file_label(&d.custom_model))
            }
        } else {
            let mb = MODELS[i].1;
            let tag = if *id == rec { "   ★ recommended" } else { "" };
            if engine::is_ipa(id) {
                let ready = if engine::ipa_runtime_ready() && engine::powsm_model_ready(&d.models_dir) { "   ✓ ready" } else { "" };
                format!("{id}  —  IPA phones (POWSM) · {}+ extra downloads{ready}", fmt_mb(mb))
            } else {
                let have = if engine::model_path_in(&d.models_dir, id).exists() { "   ✓ downloaded" } else { "" };
                format!("{id}  —  {}{have}{tag}", fmt_mb(mb))
            }
        };
        SendMessageW(cb, CB_ADDSTRING, 0, w(&label).as_ptr() as isize);
        if *id == d.model {
            SendMessageW(cb, CB_SETCURSEL, i, 0);
        }
    }
}

unsafe fn fill_text(cb: HWND, d: &Config) {
    SendMessageW(cb, CB_RESETCONTENT, 0, 0);
    let rec = llm::recommended_text();
    let budget = llm::budget_mb();
    for (i, id) in text_ids().iter().enumerate() {
        let label = if *id == CUSTOM {
            if d.custom_text.as_os_str().is_empty() {
                "Custom text model file (.gguf)…".to_string()
            } else {
                format!("Custom: {}", file_label(&d.custom_text))
            }
        } else {
            let m = &TEXT_MODELS[i];
            let have = if d.text_dir.join(m.file).exists() { "   ✓ downloaded" } else { "" };
            let tag = if i == rec {
                "   ★ recommended".to_string()
            } else if llm::est_loaded_mb(m.mb) > budget {
                "   ⚠ above your recommended limit".to_string()
            } else {
                String::new()
            };
            format!("{}  —  {}{have}{tag}", m.label, fmt_mb(m.mb))
        };
        SendMessageW(cb, CB_ADDSTRING, 0, w(&label).as_ptr() as isize);
        if *id == d.text_model {
            SendMessageW(cb, CB_SETCURSEL, i, 0);
        }
    }
}

// ---- the "extra download" warning ---------------------------------------------------------------------
/// Is everything the chosen model + AI mode needs already on disk?
pub fn components_ready(c: &Config) -> bool {
    (!engine::is_ipa(&c.model) || (engine::ipa_runtime_ready() && engine::powsm_model_ready(&c.models_dir)))
        && (c.ai_mode == AiMode::Off || (llm::llama_ready() && llm::text_model_ready(c)))
}

/// Does `c` need any extra component at all (IPA runtime and/or text AI)?
pub fn needs_extras(c: &Config) -> bool {
    engine::is_ipa(&c.model) || c.ai_mode != AiMode::Off
}

/// IPA needs a Python/PyTorch runtime; any AI mode needs the text engine + a Gemma model. Nothing is
/// downloaded until the user says yes.
pub unsafe fn confirm_extra_download(owner: HWND, c: &Config) -> bool {
    let mut parts: Vec<String> = Vec::new();
    let (mut mb, mut peak) = (0u32, 0u32); // peak: wheels are cached while installing, then deleted
    let ipa = engine::is_ipa(&c.model);
    if ipa && !engine::ipa_runtime_ready() {
        let (r, kind) = if engine::has_nvidia() {
            (engine::IPA_RUNTIME_MB, "CUDA GPU build")
        } else {
            (engine::IPA_RUNTIME_CPU_MB, "CPU build")
        };
        mb += r;
        peak += r * 2;
        parts.push(format!("  •  Python + PyTorch ({kind}) + ESPnet runtime: about {}\n      → {}\\ipa", fmt_mb(r), util::data_dir().display()));
    }
    if ipa && !engine::powsm_model_ready(&c.models_dir) {
        mb += 1311;
        peak += 1311;
        parts.push(format!("  •  POWSM phone model: about 1.3 GB\n      → {}\\powsm", c.models_dir.display()));
    }
    if c.ai_mode != AiMode::Off {
        if !llm::llama_ready() {
            mb += llm::llama_mb();
            peak += llm::llama_mb();
            parts.push(format!("  •  Text engine (llama.cpp): about {}\n      → {}", fmt_mb(llm::llama_mb()), llm::llama_dir().display()));
        }
        if !llm::text_model_ready(c) {
            if let Some(m) = llm::text_model(&c.text_model) {
                mb += m.mb;
                peak += m.mb;
                parts.push(format!("  •  Text AI model ({}): about {}\n      → {}", m.label, fmt_mb(m.mb), c.text_dir.display()));
            }
        }
    }
    let ai_note = if c.ai_mode == AiMode::Off {
        "\n\nAI is off: you'll get raw IPA phones.".to_string()
    } else if ipa {
        "\n\n⚠  IPA + AI is experimental. The AI can hallucinate: it may invent, change or \"fix\" words that weren't said. It is designed for multilingual or unreliable speech (several languages, accents, noisy audio). For normal single-language dictation a regular Whisper model is more accurate.".to_string()
    } else {
        format!("\n\n⚠  \"{}\" runs your dictation through a local AI. It can change or invent words (Simple makes minimal edits; Rambler and Cliffnotes rewrite a lot), so check important text before you use it.", c.ai_mode.id())
    };
    let body = if parts.is_empty() { "Everything needed is already installed.".to_string() } else { parts.join("\n") };
    let text = format!(
        "This needs extra components that plain Whisper dictation doesn't use:\n\n{body}\n\nTotal download: about {} (needs up to {} of free disk space while installing). This can take 10+ minutes. Nothing is installed until you click Yes, and \"Delete all data…\" removes it again.{ai_note}\n\nDownload and install now?",
        fmt_mb(mb),
        fmt_mb(peak),
    );
    MessageBoxW(owner, w(&text).as_ptr(), w("Extra download").as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2) == IDYES
}

/// Removes everything SimpleSTT put on disk (engines, runtime, models it downloaded, settings, caches).
pub fn wipe(c: &Config) {
    engine::cancel();
    std::thread::sleep(Duration::from_millis(400));
    let dd = util::data_dir();
    if !c.models_dir.starts_with(&dd) {
        // custom folder: delete only the files we created there, never the user's other files
        engine::powsm_remove(&c.models_dir);
        if let Ok(rd) = std::fs::read_dir(&c.models_dir) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().to_lowercase();
                if n.starts_with("ggml-") && (n.ends_with(".bin") || n.ends_with(".bin.part")) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&c.models_dir);
    }
    if !c.text_dir.starts_with(&dd) {
        llm::remove_text_models(&c.text_dir);
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

// ---- common combo/browse handling (settings + wizard) ----------------------------------------------------
const ID_DIR: i32 = 101;
const ID_BROWSE: i32 = 102;
const ID_MODEL: i32 = 103;
const ID_LANG: i32 = 104;
const ID_TDIR: i32 = 105;
const ID_TBROWSE: i32 = 106;
const ID_DELETE: i32 = 107;
const ID_TMODEL: i32 = 108;
const ID_AI: i32 = 109; // the AI-mode combo

fn sel_index(cb: HWND) -> usize {
    unsafe { SendMessageW(cb, CB_GETCURSEL, 0, 0).max(0) as usize }
}

/// A combo changed: if "custom" was picked, ask for the file (restoring the old choice on cancel);
/// otherwise remember the choice in the draft. `which` 0 = whisper, 1 = text.
unsafe fn on_combo_changed(owner: HWND, cb: HWND, which: usize) {
    let idx = sel_index(cb);
    let ids = if which == 0 { whisper_ids() } else { text_ids() };
    let id = ids[idx.min(ids.len() - 1)];
    if id == CUSTOM {
        let picked = if which == 0 {
            pick_file(owner, "Choose a Whisper model (ggml .bin)", "Whisper models (*.bin)\0*.bin\0All files\0*.*")
        } else {
            pick_file(owner, "Choose a text model (.gguf)", "GGUF models (*.gguf)\0*.gguf\0All files\0*.*")
        };
        match picked {
            Some(p) => set_draft(|d| {
                if which == 0 {
                    d.custom_model = p;
                    d.model = CUSTOM.into();
                } else {
                    d.custom_text = p;
                    d.text_model = CUSTOM.into();
                }
            }),
            None => {
                SendMessageW(cb, CB_SETCURSEL, PREV_SEL.lock().unwrap()[which], 0); // back to what it was
                return;
            }
        }
    } else {
        PREV_SEL.lock().unwrap()[which] = idx;
        set_draft(|d| {
            if which == 0 {
                d.model = id.to_string();
            } else {
                d.text_model = id.to_string();
            }
        });
    }
    let d = draft();
    if which == 0 {
        fill_whisper(cb, &d);
    } else {
        fill_text(cb, &d);
    }
}

fn register(name: &str, proc_: WNDPROC, bg: HBRUSH, once: &Once) {
    once.call_once(|| unsafe {
        let cls = w(name); // must outlive RegisterClassW
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: proc_,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: GetModuleHandleW(null()),
            hIcon: LoadIconW(GetModuleHandleW(null()), 1usize as *const u16),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: bg,
            lpszMenuName: null(),
            lpszClassName: cls.as_ptr(),
        };
        RegisterClassW(&wc);
    });
}

unsafe fn create_dialog(parent: HWND, class: &str, title: &str, cw: i32, ch: i32) -> HWND {
    let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
    let mut r = RECT { left: 0, top: 0, right: s(cw), bottom: s(ch) };
    AdjustWindowRectEx(&mut r, style, 0, 0);
    let (ww, wh) = (r.right - r.left, r.bottom - r.top);
    let mut pr: RECT = std::mem::zeroed();
    GetWindowRect(parent, &mut pr);
    let x = ((pr.left + pr.right - ww) / 2).max(0);
    let y = ((pr.top + pr.bottom - wh) / 2).max(0);
    CreateWindowExW(0, w(class).as_ptr(), w(title).as_ptr(), style, x, y, ww, wh, parent, null_mut(), GetModuleHandleW(null()), null())
}

// ---- settings window ------------------------------------------------------------------------------------
pub unsafe fn open_settings(parent: HWND) {
    let existing = active_dialog();
    if !existing.is_null() {
        SetForegroundWindow(existing);
        return;
    }
    static REG: Once = Once::new();
    register(&util::class("SimpleSTTSettings"), Some(settings_proc), (COLOR_BTNFACE + 1) as HBRUSH, &REG);
    *DRAFT.lock().unwrap() = Some(cfg());
    let h = create_dialog(parent, &util::class("SimpleSTTSettings"), "SimpleSTT settings", 460, 440);
    SETTINGS.store(h as isize, Relaxed);
    // Non-modal: the main window stays usable. The settings window is *owned* by it, so it always stays
    // above the main window instead of disappearing behind it.
    ShowWindow(h, SW_SHOW);
}

unsafe extern "system" fn settings_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            F_UI.store(make_font("Segoe UI", 12, 400) as isize, Relaxed);
            let c = draft();
            ctl(hwnd, "STATIC", "Whisper models folder", 0, 16, 12, 428, 18, 0);
            ctl(hwnd, "EDIT", &c.models_dir.to_string_lossy(), EDIT, 16, 30, 340, 24, ID_DIR);
            ctl(hwnd, "BUTTON", "Browse…", BTN, 364, 29, 80, 26, ID_BROWSE);
            ctl(hwnd, "STATIC", "Model  (downloaded automatically when selected)", 0, 16, 64, 428, 18, 0);
            let cb = ctl(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | 3 /* CBS_DROPDOWNLIST */, 16, 82, 428, 300, ID_MODEL);
            fill_whisper(cb, &c);
            PREV_SEL.lock().unwrap()[0] = sel_index(cb);
            ctl(hwnd, "STATIC", "Language  (auto, en, de, fr, es, … — .en models always use English)", 0, 16, 116, 428, 18, 0);
            ctl(hwnd, "EDIT", &c.lang, EDIT, 16, 134, 120, 24, ID_LANG);

            ctl(hwnd, "STATIC", "Text AI mode  (local Gemma model — can hallucinate)", 0, 16, 172, 428, 18, 0);
            let ai = ctl(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | 3, 16, 192, 428, 200, ID_AI);
            for m in AiMode::ALL {
                SendMessageW(ai, CB_ADDSTRING, 0, w(m.label()).as_ptr() as isize);
            }
            SendMessageW(ai, CB_SETCURSEL, AiMode::ALL.iter().position(|x| *x == c.ai_mode).unwrap_or(0), 0);
            ctl(hwnd, "STATIC", "Text models folder  (used by every mode except Off)", 0, 16, 234, 428, 18, 0);
            ctl(hwnd, "EDIT", &c.text_dir.to_string_lossy(), EDIT, 16, 252, 340, 24, ID_TDIR);
            ctl(hwnd, "BUTTON", "Browse…", BTN, 364, 251, 80, 26, ID_TBROWSE);
            ctl(hwnd, "STATIC", "Text model  (up to 5 GB is loaded in memory)", 0, 16, 286, 428, 18, 0);
            let tcb = ctl(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | 3, 16, 304, 428, 300, ID_TMODEL);
            fill_text(tcb, &c);
            PREV_SEL.lock().unwrap()[1] = sel_index(tcb);

            let dd = format!("Engines, caches and settings live in:\n{}", util::data_dir().display());
            ctl(hwnd, "STATIC", &dd, 0, 16, 342, 428, 36, 0);
            ctl(hwnd, "BUTTON", "Delete all data…", BTN, 16, 394, 130, 30, ID_DELETE);
            ctl(hwnd, "BUTTON", "Save", BTN | 1 /* BS_DEFPUSHBUTTON */, 274, 394, 80, 30, 1);
            ctl(hwnd, "BUTTON", "Cancel", BTN, 364, 394, 80, 30, 2);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xffff) as i32;
            let code = ((wp >> 16) & 0xffff) as u32;
            let dlg = |i| GetDlgItem(hwnd, i);
            match id {
                ID_BROWSE => {
                    if let Some(p) = browse_folder(hwnd, "Choose where to store Whisper models") {
                        SetWindowTextW(dlg(ID_DIR), w(&p.to_string_lossy()).as_ptr());
                        set_draft(|d| d.models_dir = p);
                        fill_whisper(dlg(ID_MODEL), &draft());
                    }
                }
                ID_TBROWSE => {
                    if let Some(p) = browse_folder(hwnd, "Choose where to store text (Gemma) models") {
                        SetWindowTextW(dlg(ID_TDIR), w(&p.to_string_lossy()).as_ptr());
                        set_draft(|d| d.text_dir = p);
                        fill_text(dlg(ID_TMODEL), &draft());
                    }
                }
                ID_DIR if code == EN_KILLFOCUS as u32 => {
                    let t = get_text(dlg(ID_DIR));
                    if !t.trim().is_empty() {
                        set_draft(|d| d.models_dir = PathBuf::from(t.trim()));
                        fill_whisper(dlg(ID_MODEL), &draft());
                    }
                }
                ID_TDIR if code == EN_KILLFOCUS as u32 => {
                    let t = get_text(dlg(ID_TDIR));
                    if !t.trim().is_empty() {
                        set_draft(|d| d.text_dir = PathBuf::from(t.trim()));
                        fill_text(dlg(ID_TMODEL), &draft());
                    }
                }
                ID_AI if code == CBN_SELCHANGE as u32 => {
                    let i = sel_index(dlg(ID_AI)).min(3);
                    set_draft(|d| d.ai_mode = AiMode::ALL[i]);
                }
                ID_MODEL if code == CBN_SELCHANGE as u32 => on_combo_changed(hwnd, dlg(ID_MODEL), 0),
                ID_TMODEL if code == CBN_SELCHANGE as u32 => on_combo_changed(hwnd, dlg(ID_TMODEL), 1),
                1 => {
                    let old = cfg();
                    let mut new = draft();
                    let dir = get_text(dlg(ID_DIR));
                    if !dir.trim().is_empty() {
                        new.models_dir = PathBuf::from(dir.trim());
                    }
                    let tdir = get_text(dlg(ID_TDIR));
                    if !tdir.trim().is_empty() {
                        new.text_dir = PathBuf::from(tdir.trim());
                    }
                    let lang = get_text(dlg(ID_LANG)).trim().to_lowercase();
                    new.lang = if lang.is_empty() { "auto".into() } else { lang };
                    new.ai_mode = AiMode::ALL[sel_index(dlg(ID_AI)).min(3)];
                    new.desktop = old.desktop; // toggled on the main window
                    new.onboarded = true;
                    // a custom entry without a file behaves like the previous choice
                    if new.model == CUSTOM && new.custom_model.as_os_str().is_empty() {
                        new.model = old.model.clone();
                    }
                    if new.text_model == CUSTOM && new.custom_text.as_os_str().is_empty() {
                        new.text_model = old.text_model.clone();
                    }
                    // IPA / AI need extra components: ask first, install nothing without a yes
                    let need_dl = needs_extras(&new) && !components_ready(&new);
                    if need_dl {
                        if !confirm_extra_download(hwnd, &new) {
                            return 0; // stay in settings; nothing saved, nothing downloaded
                        }
                        engine::IPA_CONSENT.store(true, Relaxed);
                    }
                    if new != old || need_dl {
                        let restart = new.engine_differs(&old) || need_dl;
                        apply_config(new, restart);
                    }
                    DestroyWindow(hwnd);
                }
                2 => {
                    DestroyWindow(hwnd);
                }
                ID_DELETE => {
                    let text = w("This deletes the speech and text engines, the IPA runtime, every model SimpleSTT downloaded, and all settings, then closes SimpleSTT.\n\nModel files you picked yourself (custom) and SimpleSTT.exe are not touched — delete those yourself if you want.\n\nContinue?");
                    let ok = MessageBoxW(hwnd, text.as_ptr(), w("Delete all SimpleSTT data").as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2);
                    if ok == IDYES {
                        // deleting a multi-GB runtime takes a while: do it off the UI thread, then close
                        let c = cfg();
                        let main = util::main_hwnd();
                        DestroyWindow(hwnd);
                        EnableWindow(main, 0); // lock the main window while we delete
                        engine::set_status("Deleting everything…", false);
                        std::thread::spawn(move || {
                            wipe(&c);
                            unsafe { PostMessageW(util::main_hwnd(), WM_CLOSE, 0, 0) };
                        });
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
            *DRAFT.lock().unwrap() = None;
            let main = util::main_hwnd();
            if IsWindow(main) != 0 && IsWindowEnabled(main) != 0 {
                SetForegroundWindow(main);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- first-run wizard ---------------------------------------------------------------------------------------
// 1 Whisper folder → 2 Whisper model → (IPA only:) 3 text folder → 4 text model. Only shown on a brand-new install.
const ID_BACK: i32 = 120;
const ID_NEXT: i32 = 121;
const ID_CANCEL: i32 = 122;
const ID_HINT: i32 = 123;
static PAGE: AtomicIsize = AtomicIsize::new(0);

pub unsafe fn open_wizard(parent: HWND) {
    static REG: Once = Once::new();
    register(&util::class("SimpleSTTWizard"), Some(wizard_proc), (COLOR_BTNFACE + 1) as HBRUSH, &REG);
    let mut d = cfg();
    d.model = llm::recommended_whisper().to_string();
    d.text_model = TEXT_MODELS[llm::recommended_text()].id.to_string();
    *DRAFT.lock().unwrap() = Some(d);
    PAGE.store(0, Relaxed);
    let h = create_dialog(parent, &util::class("SimpleSTTWizard"), "Welcome to SimpleSTT", 480, 290);
    WIZARD.store(h as isize, Relaxed);
    ShowWindow(h, SW_SHOW);
    SetForegroundWindow(h);
}

/// controls belonging to each page
fn page_ids(p: isize) -> &'static [i32] {
    match p {
        0 => &[ID_DIR, ID_BROWSE],
        1 => &[ID_MODEL],
        2 => &[ID_TDIR, ID_TBROWSE],
        _ => &[ID_TMODEL],
    }
}

unsafe fn show_page(hwnd: HWND, page: isize) {
    PAGE.store(page, Relaxed);
    for p in 0..4 {
        for id in page_ids(p) {
            ShowWindow(GetDlgItem(hwnd, *id), if p == page { SW_SHOW } else { SW_HIDE });
        }
    }
    let (title, hint) = match page {
        0 => (
            "Step 1: where should Whisper speech models be stored?".to_string(),
            "Models are downloaded here the first time you use them. You can change this later in settings.".to_string(),
        ),
        1 => (
            "Step 2: choose a Whisper model.".to_string(),
            format!(
                "★ marks the recommended model for this PC ({}). Pick \"custom\" to use your own ggml .bin file. powsm-ipa (IPA phones + AI) is experimental and needs ~9 GB of extra downloads — you'll be asked before anything is installed.",
                match llm::vram_mb() {
                    Some(v) => format!("NVIDIA GPU with {} VRAM, {} RAM", fmt_mb(v), fmt_mb(llm::ram_mb())),
                    None => format!("no NVIDIA GPU, {} RAM", fmt_mb(llm::ram_mb())),
                }
            ),
        ),
        2 => (
            "Step 3: where should text (Gemma) models be stored?".to_string(),
            "IPA mode uses a small Gemma 4 model to write the phones out as text. Models are downloaded here when you finish.".to_string(),
        ),
        _ => (
            "Step 4: choose a Gemma model.".to_string(),
            format!(
                "★ is the largest model that fits in half your {} (max 5 GB loaded). Bigger ones are listed too if you want to try them. Remember: IPA + AI can hallucinate and is meant for multilingual or unreliable speech.",
                if llm::vram_mb().is_some() { "VRAM" } else { "RAM" }
            ),
        ),
    };
    SetWindowTextW(GetDlgItem(hwnd, 130), w(&title).as_ptr());
    SetWindowTextW(GetDlgItem(hwnd, ID_HINT), w(&hint).as_ptr());
    EnableWindow(GetDlgItem(hwnd, ID_BACK), (page > 0) as i32);
    let last = page == 3 || (page == 1 && !engine::is_ipa(&draft().model));
    SetWindowTextW(GetDlgItem(hwnd, ID_NEXT), w(if last { "Finish" } else { "Next" }).as_ptr());
    InvalidateRect(hwnd, null(), 1);
}

unsafe extern "system" fn wizard_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            F_UI.store(make_font("Segoe UI", 12, 400) as isize, Relaxed);
            let d = draft();
            ctl(hwnd, "STATIC", "", 0, 16, 16, 448, 20, 130);
            // page 0 / 2: folders
            ctl(hwnd, "EDIT", &d.models_dir.to_string_lossy(), EDIT, 16, 52, 360, 24, ID_DIR);
            ctl(hwnd, "BUTTON", "Browse…", BTN, 384, 51, 80, 26, ID_BROWSE);
            ctl(hwnd, "EDIT", &d.text_dir.to_string_lossy(), EDIT, 16, 52, 360, 24, ID_TDIR);
            ctl(hwnd, "BUTTON", "Browse…", BTN, 384, 51, 80, 26, ID_TBROWSE);
            // page 1 / 3: model pickers
            let cb = ctl(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | 3, 16, 52, 448, 300, ID_MODEL);
            fill_whisper(cb, &d);
            PREV_SEL.lock().unwrap()[0] = sel_index(cb);
            let tcb = ctl(hwnd, "COMBOBOX", "", WS_TABSTOP | WS_VSCROLL | 3, 16, 52, 448, 300, ID_TMODEL);
            fill_text(tcb, &d);
            PREV_SEL.lock().unwrap()[1] = sel_index(tcb);
            ctl(hwnd, "STATIC", "", 0, 16, 100, 448, 110, ID_HINT);
            ctl(hwnd, "BUTTON", "Cancel", BTN, 16, 244, 80, 30, ID_CANCEL);
            ctl(hwnd, "BUTTON", "Back", BTN, 290, 244, 80, 30, ID_BACK);
            ctl(hwnd, "BUTTON", "Next", BTN | 1, 384, 244, 80, 30, ID_NEXT);
            show_page(hwnd, 0);
            0
        }
        WM_COMMAND => {
            let id = (wp & 0xffff) as i32;
            let code = ((wp >> 16) & 0xffff) as u32;
            let dlg = |i| GetDlgItem(hwnd, i);
            match id {
                ID_BROWSE => {
                    if let Some(p) = browse_folder(hwnd, "Choose where to store Whisper models") {
                        SetWindowTextW(dlg(ID_DIR), w(&p.to_string_lossy()).as_ptr());
                    }
                }
                ID_TBROWSE => {
                    if let Some(p) = browse_folder(hwnd, "Choose where to store text (Gemma) models") {
                        SetWindowTextW(dlg(ID_TDIR), w(&p.to_string_lossy()).as_ptr());
                    }
                }
                ID_MODEL if code == CBN_SELCHANGE as u32 => {
                    on_combo_changed(hwnd, dlg(ID_MODEL), 0);
                    show_page(hwnd, 1); // Next ↔ Finish depends on the choice
                }
                ID_TMODEL if code == CBN_SELCHANGE as u32 => on_combo_changed(hwnd, dlg(ID_TMODEL), 1),
                ID_BACK => {
                    let p = PAGE.load(Relaxed);
                    show_page(hwnd, if p == 3 { 2 } else { p - 1 }.max(0));
                }
                ID_NEXT | 1 => {
                    let page = PAGE.load(Relaxed);
                    // fold the folder edits into the draft (also refreshes the "downloaded" ticks)
                    let t = get_text(dlg(ID_DIR));
                    if !t.trim().is_empty() {
                        set_draft(|d| d.models_dir = PathBuf::from(t.trim()));
                    }
                    let t = get_text(dlg(ID_TDIR));
                    if !t.trim().is_empty() {
                        set_draft(|d| d.text_dir = PathBuf::from(t.trim()));
                    }
                    fill_whisper(dlg(ID_MODEL), &draft());
                    fill_text(dlg(ID_TMODEL), &draft());
                    let d = draft();
                    let finish = page == 3 || (page == 1 && !engine::is_ipa(&d.model));
                    if !finish {
                        show_page(hwnd, page + 1);
                        return 0;
                    }
                    let mut new = d;
                    new.onboarded = true;
                    if new.model == CUSTOM && new.custom_model.as_os_str().is_empty() {
                        new.model = llm::recommended_whisper().to_string();
                    }
                    if new.text_model == CUSTOM && new.custom_text.as_os_str().is_empty() {
                        new.text_model = TEXT_MODELS[llm::recommended_text()].id.to_string();
                    }
                    if engine::is_ipa(&new.model) && new.ai_mode == AiMode::Off {
                        new.ai_mode = AiMode::Simple; // IPA without the AI is just raw phones; offer the useful default
                    }
                    if needs_extras(&new) && !components_ready(&new) {
                        if !confirm_extra_download(hwnd, &new) {
                            return 0; // stay in the wizard
                        }
                        engine::IPA_CONSENT.store(true, Relaxed);
                    }
                    apply_config(new, true);
                    DestroyWindow(hwnd);
                }
                ID_CANCEL => {
                    DestroyWindow(hwnd);
                    PostMessageW(util::main_hwnd(), WM_CLOSE, 0, 0); // not set up → quit, nothing downloaded
                }
                _ => {}
            }
            0
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            PostMessageW(util::main_hwnd(), WM_CLOSE, 0, 0);
            0
        }
        WM_DESTROY => {
            WIZARD.store(0, Relaxed);
            DeleteObject(F_UI.load(Relaxed) as HFONT);
            *DRAFT.lock().unwrap() = None;
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}
