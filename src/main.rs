#![windows_subsystem = "windows"]

mod audio;
mod dialogs;
mod engine;
mod live;
mod llm;
mod loopback;
mod net;
mod ui;
mod update;
mod util;

use std::ptr::null;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

fn main() {
    // Single instance. After a self-update the old process is still closing, so wait for it.
    let relaunch = std::env::args().any(|a| a == "--relaunch");
    let tid = util::test_id(); // empty unless SIMPLESTT_TEST_ID is set (isolated test instances)
    let name = util::w(&format!("Local\\SimpleSTT.single{tid}"));
    for attempt in 0..30 {
        unsafe {
            let h = CreateMutexW(null(), 0, name.as_ptr());
            if GetLastError() != ERROR_ALREADY_EXISTS {
                let _ = h; // intentionally held for the process lifetime
                break;
            }
            if !relaunch || attempt == 29 {
                let w = FindWindowW(util::w(&util::class("SimpleSTTWnd")).as_ptr(), null());
                if !w.is_null() {
                    ShowWindow(w, SW_RESTORE);
                    SetForegroundWindow(w);
                }
                return;
            }
            windows_sys::Win32::Foundation::CloseHandle(h);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    ui::run();
}
