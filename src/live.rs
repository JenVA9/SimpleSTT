//! Live (but invisible) transcription: while you record, a background thread watches the audio for
//! pauses and transcribes each finished phrase, appending the text to a temp file. When you press stop
//! only the unsent tail is left to transcribe, so the wait after stopping stays ~constant whether the
//! recording was ten seconds or an hour.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use crate::engine;

pub const RATE: usize = 16000;
const FRAME: usize = RATE * 30 / 1000; // 30 ms analysis frames
const GUARD: usize = RATE / 2; // never analyse the newest 0.5 s while recording (desktop-audio jitter)
const MIN_SILENCE: usize = 23; // frames (~0.7 s) of quiet that end a phrase
const MIN_VOICED: usize = 3; // frames of sound before a chunk counts as speech
/// Longest chunk (seconds) sent to the engine before a cut is forced at the quietest spot. Whisper works on
/// 30 s windows (20 s is safe); POWSM is more accurate on shorter phrases, so IPA mode lowers this.
pub static MAX_CHUNK_S: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(20);
const PRE_ROLL: usize = RATE * 3 / 10;

/// A growing 16 kHz mono stream that can drop its already-processed head, so hour-long recordings
/// don't keep hundreds of MB of audio around. Indices are global (never shift).
#[derive(Default)]
pub struct Timeline {
    base: usize,
    data: Vec<i16>,
}

impl Timeline {
    pub fn len(&self) -> usize {
        self.base + self.data.len()
    }
    pub fn push(&mut self, s: &[i16]) {
        self.data.extend_from_slice(s);
    }
    /// Append `chunk` so it starts at global index `start` (zero-filling any gap).
    pub fn place(&mut self, start: usize, chunk: &[i16]) {
        if self.len() < start {
            self.data.resize(start - self.base, 0);
        }
        self.data.extend_from_slice(chunk);
    }
    fn get(&self, i: usize) -> i16 {
        i.checked_sub(self.base).and_then(|j| self.data.get(j)).copied().unwrap_or(0)
    }
    fn discard_before(&mut self, n: usize) {
        let k = n.saturating_sub(self.base).min(self.data.len());
        self.data.drain(..k);
        self.base += k;
    }
}

#[derive(Default)]
pub struct Live {
    pub mic: Mutex<Timeline>,
    pub desk: Mutex<Timeline>,
    /// set by the recorder once the mic is closed and every sample has been delivered
    pub mic_done: AtomicBool,
}

impl Live {
    /// mic + desktop audio for global sample range [from, to)
    fn mixed(&self, from: usize, to: usize) -> Vec<i16> {
        let (m, d) = (self.mic.lock().unwrap(), self.desk.lock().unwrap());
        (from..to).map(|i| m.get(i).saturating_add(d.get(i))).collect()
    }
    fn rms(&self, start: usize) -> f32 {
        let (m, d) = (self.mic.lock().unwrap(), self.desk.lock().unwrap());
        let sum: f64 = (start..start + FRAME)
            .map(|i| {
                let v = m.get(i) as i32 + d.get(i) as i32;
                (v * v) as f64
            })
            .sum();
        (sum / FRAME as f64).sqrt() as f32
    }
    fn discard_before(&self, n: usize) {
        self.mic.lock().unwrap().discard_before(n);
        self.desk.lock().unwrap().discard_before(n);
    }
}

/// Chunks the engine could not transcribe even after restarting (their audio was saved to disk instead).
pub static LOST: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
static LAST_ERR: Mutex<String> = Mutex::new(String::new());

pub fn last_error() -> String {
    LAST_ERR.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Folder where audio is kept when the engine failed on it.
pub fn unprocessed_dir() -> std::path::PathBuf {
    crate::util::data_dir().join("unprocessed")
}

fn save_unprocessed(audio: &[i16]) {
    let dir = unprocessed_dir();
    let _ = std::fs::create_dir_all(&dir);
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let n = LOST.load(Ordering::Relaxed);
    let _ = std::fs::write(dir.join(format!("audio-{secs}-{n}.wav")), engine::wav(audio));
}

/// Transcribes one phrase. If the engine fails for good, the audio is written to disk and the recording goes
/// on, so one bad moment never throws away what was said.
fn send(
    live: &Live,
    from: usize,
    to: usize,
    transcribe: &dyn Fn(&[i16]) -> Result<String, String>,
    on_text: &dyn Fn(&str),
    text: &mut String,
) -> Result<(), String> {
    let audio = live.mixed(from, to);
    match transcribe(&audio) {
        Ok(t) => {
            if !t.is_empty() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&t);
                on_text(&t);
            }
        }
        Err(e) => {
            save_unprocessed(&audio);
            LOST.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = LAST_ERR.lock() {
                *g = e;
            }
        }
    }
    Ok(())
}

/// Runs for the whole recording; returns the full transcript once the recorder has finished.
/// `label` is the status prefix ("Listening", ...), shown with a running timer.
pub fn run(
    live: &Live,
    stop: &AtomicBool,
    label: &str,
    label_bad: bool,
    transcribe: &dyn Fn(&[i16]) -> Result<String, String>,
    on_text: &dyn Fn(&str),
) -> Result<String, String> {
    let mut text = String::new();
    let (mut pos, mut scan) = (0usize, 0usize); // pos: first unsent sample, scan: next frame to analyse
    let (mut voiced, mut silent_run, mut last_voice_end) = (0usize, 0usize, 0usize);
    let mut floor = 150.0f32; // running estimate of the room's noise level
    let mut shown_secs = u64::MAX;

    loop {
        let stopping = stop.load(Ordering::Acquire);
        let done = live.mic_done.load(Ordering::Acquire);
        if stopping && !done {
            std::thread::sleep(Duration::from_millis(20)); // recorder is still flushing its last buffers
            continue;
        }
        let head = live.mic.lock().unwrap().len();
        let limit = if done { head } else { head.saturating_sub(GUARD) };

        while scan + FRAME <= limit {
            let rms = live.rms(scan);
            floor = if rms < floor { rms } else { floor * 0.999 + rms * 0.001 };
            let loud = rms > (floor * 3.0).max(150.0);
            scan += FRAME;
            if loud {
                voiced += 1;
                silent_run = 0;
                last_voice_end = scan;
            } else {
                silent_run += 1;
            }

            if voiced == 0 {
                // nothing said yet: keep only a short pre-roll so silence never piles up
                let keep = scan.saturating_sub(PRE_ROLL);
                if keep > pos {
                    pos = keep;
                    live.discard_before(pos);
                }
            } else if voiced >= MIN_VOICED && silent_run >= MIN_SILENCE {
                // a phrase just ended: transcribe it now, while the user keeps talking
                let end = (last_voice_end + RATE / 5).min(scan);
                send(live, pos, end, transcribe, on_text, &mut text)?;
                pos = scan;
                live.discard_before(pos);
                (voiced, silent_run) = (0, 0);
            } else if scan - pos >= MAX_CHUNK_S.load(Ordering::Relaxed) * RATE {
                // non-stop sound (e.g. music under a call): cut at the quietest spot of the last 4 s
                let from = scan.saturating_sub(4 * RATE).max(pos + RATE);
                let mut best = (f32::MAX, scan);
                let mut f = from;
                while f + FRAME <= scan {
                    let e = live.rms(f);
                    if e < best.0 {
                        best = (e, f);
                    }
                    f += FRAME;
                }
                send(live, pos, best.1, transcribe, on_text, &mut text)?;
                pos = best.1;
                live.discard_before(pos);
                (voiced, silent_run) = (MIN_VOICED, 0);
            }
        }

        if done {
            if voiced >= MIN_VOICED {
                send(live, pos, head, transcribe, on_text, &mut text)?; // the unsent tail
            }
            return Ok(text);
        }

        if engine::STATE.load(Ordering::Relaxed) == engine::REC {
            let secs = (head / RATE) as u64;
            if secs != shown_secs {
                shown_secs = secs;
                engine::set_status(
                    &format!("{label}… {}:{:02} · click to finish", secs / 60, secs % 60),
                    label_bad,
                );
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Offline harness (needs a running whisper-server): feeds a wav straight into the timeline and compares
/// chunked live transcription with one-shot transcription.
///   WAV=... PORT=8181 OUT=prefix cargo test --release live_vs_oneshot -- --ignored --nocapture
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn read_wav(path: &str) -> Vec<i16> {
        let b = std::fs::read(path).unwrap();
        let mut i = 12;
        while i + 8 <= b.len() {
            let n = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
            if &b[i..i + 4] == b"data" {
                let end = (i + 8 + n).min(b.len());
                return b[i + 8..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            }
            i += 8 + n;
        }
        panic!("no data chunk");
    }

    #[test]
    #[ignore]
    fn live_vs_oneshot() {
        let samples = read_wav(&std::env::var("WAV").unwrap());
        engine::test_set_port(std::env::var("PORT").unwrap().parse().unwrap());
        let out = std::env::var("OUT").unwrap();
        println!("audio: {:.1}s", samples.len() as f32 / RATE as f32);

        let t = Instant::now();
        let one = engine::transcribe(&samples).unwrap();
        println!("one-shot: {:.2}s, {} chars", t.elapsed().as_secs_f32(), one.len());
        std::fs::write(format!("{out}_oneshot.txt"), &one).unwrap();

        let live = Live::default();
        live.mic.lock().unwrap().push(&samples);
        live.mic_done.store(true, Ordering::Release);
        let stop = AtomicBool::new(true);
        let chunks = std::cell::Cell::new(0);
        let t = Instant::now();
        let text = run(&live, &stop, "x", false, &|a| {
            chunks.set(chunks.get() + 1);
            println!("  chunk {:>2}: {:5.1}s", chunks.get(), a.len() as f32 / RATE as f32);
            engine::transcribe(a)
        }, &|_| {})
        .unwrap();
        println!("live total: {:.2}s over {} chunks, {} chars", t.elapsed().as_secs_f32(), chunks.get(), text.len());
        std::fs::write(format!("{out}_live.txt"), &text).unwrap();
    }

    /// Real-time-ish: audio arrives in 100 ms pieces (SPEED x faster than real time) while the chunker runs
    /// concurrently, then "stop" is pressed and we measure how long the tail takes. NOISE adds room noise (rms).
    #[test]
    #[ignore]
    fn realtime() {
        let mut samples = read_wav(&std::env::var("WAV").unwrap());
        engine::test_set_port(std::env::var("PORT").unwrap().parse().unwrap());
        let out = std::env::var("OUT").unwrap();
        let speed: f32 = std::env::var("SPEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let noise: f32 = std::env::var("NOISE").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        if noise > 0.0 {
            let mut x = 12345u32;
            for v in samples.iter_mut() {
                x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                let r = ((x >> 16) as f32 / 32768.0 - 1.0) * noise * 1.7320508; // uniform, rms = noise
                *v = (*v as f32 + r).clamp(-32768.0, 32767.0) as i16;
            }
        }
        let live = Live::default();
        let stop = AtomicBool::new(false);
        let chunks = std::sync::atomic::AtomicUsize::new(0);
        let secs = samples.len() as f32 / RATE as f32;
        let (text, tail) = std::thread::scope(|sc| {
            let (live, stop, chunks) = (&live, &stop, &chunks);
            let samples = &samples;
            let worker = sc.spawn(move || {
                run(live, stop, "x", false, &|a| {
                    chunks.fetch_add(1, Ordering::Relaxed);
                    engine::transcribe(a)
                }, &|_| {})
                .unwrap()
            });
            for piece in samples.chunks(RATE / 10) {
                live.mic.lock().unwrap().push(piece);
                std::thread::sleep(Duration::from_secs_f32(0.1 / speed));
            }
            let t = Instant::now(); // <- the user presses stop here
            stop.store(true, Ordering::Release);
            live.mic_done.store(true, Ordering::Release);
            let text = worker.join().unwrap();
            (text, t.elapsed().as_secs_f32())
        });
        println!("audio {:.0}s speed x{} noise {} -> stop-to-text {:.2}s, {} chunks, {} chars", secs, speed, noise, tail, chunks.load(Ordering::Relaxed), text.len());
        std::fs::write(format!("{out}_rt.txt"), &text).unwrap();
    }
}
