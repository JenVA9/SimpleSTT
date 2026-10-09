//! "Desktop audio" capture: WASAPI loopback of the default playback device (the other side of a call,
//! a video, ...). The device delivers its own mix format (usually 48 kHz float stereo); we downmix and
//! decimate to Whisper's 16 kHz mono and drop it onto a wall-clock timeline, because loopback only
//! produces packets while something is actually playing.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::*;
use crate::live::Timeline;
use windows::Win32::System::Com::*;

const OUT_RATE: f64 = 16000.0;

/// Runs until `stop` is set. `opened` is signalled exactly once, after the stream starts (or fails).
pub fn run(stop: &AtomicBool, t0: Instant, out: &Mutex<Timeline>, opened: &dyn Fn(Result<(), String>)) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        match capture(stop, t0, out, opened) {
            Ok(()) => {}
            Err(e) => opened(Err(e.message())), // no-op if already signalled
        }
        CoUninitialize();
    }
}

unsafe fn capture(
    stop: &AtomicBool,
    t0: Instant,
    out: &Mutex<Timeline>,
    opened: &dyn Fn(Result<(), String>),
) -> windows::core::Result<()> {
    let en: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let dev = en.GetDefaultAudioEndpoint(eRender, eConsole)?;
    let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
    let fmt = client.GetMixFormat()?;

    // WAVEFORMATEX is packed; copy fields out by value.
    let tag = std::ptr::addr_of!((*fmt).wFormatTag).read_unaligned();
    let ch = std::ptr::addr_of!((*fmt).nChannels).read_unaligned() as usize;
    let rate = std::ptr::addr_of!((*fmt).nSamplesPerSec).read_unaligned() as f64;
    let bits = std::ptr::addr_of!((*fmt).wBitsPerSample).read_unaligned() as usize;
    let block = std::ptr::addr_of!((*fmt).nBlockAlign).read_unaligned() as usize;
    // WAVE_FORMAT_EXTENSIBLE (0xFFFE): sub-format GUID's first dword at offset 24 (3 = float, 1 = PCM)
    let is_float = tag == 3
        || (tag == 0xFFFE && (fmt as *const u8).add(24).cast::<u32>().read_unaligned() == 3);

    client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 2_000_000, 0, fmt, None)?;
    let cap: IAudioCaptureClient = client.GetService()?;
    client.Start()?;
    opened(Ok(()));

    let step = rate / OUT_RATE; // source frames per output sample
    let (mut pos, mut acc, mut n) = (0.0f64, 0.0f32, 0u32);
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(10));
        loop {
            if cap.GetNextPacketSize()? == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut::<u8>(), 0u32, 0u32);
            cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            let mut chunk: Vec<i16> = Vec::with_capacity((frames as f64 / step) as usize + 2);
            for f in 0..frames as usize {
                let mut m = 0.0f32;
                if !silent {
                    let base = data.add(f * block);
                    for c in 0..ch {
                        m += match (is_float, bits) {
                            (true, 32) => base.add(c * 4).cast::<f32>().read_unaligned(),
                            (false, 16) => base.add(c * 2).cast::<i16>().read_unaligned() as f32 / 32768.0,
                            (false, 24) => {
                                let p = base.add(c * 3);
                                let v = (*p as i32) | ((*p.add(1) as i32) << 8) | ((*p.add(2) as i8 as i32) << 16);
                                v as f32 / 8_388_608.0
                            }
                            (false, 32) => base.add(c * 4).cast::<i32>().read_unaligned() as f32 / 2_147_483_648.0,
                            _ => 0.0,
                        };
                    }
                    m /= ch as f32;
                }
                acc += m;
                n += 1;
                pos += 1.0;
                if pos >= step {
                    pos -= step;
                    chunk.push(((acc / n as f32).clamp(-1.0, 1.0) * 32767.0) as i16);
                    acc = 0.0;
                    n = 0;
                }
            }
            cap.ReleaseBuffer(frames)?;
            if !chunk.is_empty() {
                // Place the chunk so it ends "now"; gaps while nothing plays become silence.
                let now = (t0.elapsed().as_secs_f64() * OUT_RATE) as usize;
                out.lock().unwrap().place(now.saturating_sub(chunk.len()), &chunk);
            }
        }
    }
    client.Stop()?;
    Ok(())
}
