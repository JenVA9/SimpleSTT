//! Microphone capture with the classic waveIn API: Windows resamples/mixes to exactly what
//! Whisper wants (16 kHz, mono, 16-bit) for us, so there is no DSP code here at all.
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Media::Audio::*;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const NBUF: usize = 6;
const BUF_BYTES: usize = 3200; // 100 ms

fn fmt() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: 16000,
        nAvgBytesPerSec: 32000,
        nBlockAlign: 2,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Blocks until `stop` is set. `opened` is signalled once the device is open (or failed).
pub fn record(stop: &AtomicBool, opened: &dyn Fn(Result<(), String>)) -> Vec<i16> {
    let mut samples: Vec<i16> = Vec::with_capacity(16000 * 30);
    unsafe {
        let ev = CreateEventW(null_mut(), 0, 0, null_mut());
        let mut dev: HWAVEIN = null_mut();
        let f = fmt();
        let r = waveInOpen(&mut dev, WAVE_MAPPER, &f, ev as usize, 0, CALLBACK_EVENT);
        if r != 0 {
            CloseHandle(ev);
            opened(Err("Couldn't open the microphone".into()));
            return samples;
        }
        let mut bufs: Vec<Vec<u8>> = (0..NBUF).map(|_| vec![0u8; BUF_BYTES]).collect();
        let mut hdrs: Box<[WAVEHDR; NBUF]> = Box::new(std::mem::zeroed());
        let hs = std::mem::size_of::<WAVEHDR>() as u32;
        for i in 0..NBUF {
            hdrs[i].lpData = bufs[i].as_mut_ptr();
            hdrs[i].dwBufferLength = BUF_BYTES as u32;
            waveInPrepareHeader(dev, &mut hdrs[i], hs);
            waveInAddBuffer(dev, &mut hdrs[i], hs);
        }
        waveInStart(dev);
        opened(Ok(()));

        let drain = |hdrs: &mut [WAVEHDR; NBUF], samples: &mut Vec<i16>, requeue: bool| {
            for i in 0..NBUF {
                // WAVEHDR is #pragma pack(1): read the driver-updated flags without a reference.
                if std::ptr::addr_of!(hdrs[i].dwFlags).read_unaligned() & WHDR_DONE != 0 {
                    let n = hdrs[i].dwBytesRecorded as usize;
                    let data = std::slice::from_raw_parts(hdrs[i].lpData, n);
                    samples.extend(data.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
                    hdrs[i].dwFlags &= !WHDR_DONE;
                    hdrs[i].dwBytesRecorded = 0;
                    if requeue {
                        waveInAddBuffer(dev, &mut hdrs[i], hs);
                    }
                }
            }
        };

        while !stop.load(Ordering::Acquire) {
            WaitForSingleObject(ev, 50);
            drain(&mut hdrs, &mut samples, true);
        }
        waveInStop(dev);
        waveInReset(dev); // returns any partially filled buffers as DONE
        drain(&mut hdrs, &mut samples, false);
        for i in 0..NBUF {
            waveInUnprepareHeader(dev, &mut hdrs[i], hs);
        }
        waveInClose(dev);
        CloseHandle(ev);
    }
    samples
}
