# SimpleSTT

A tiny (~300 KB) Windows speech-to-text widget. Click the mic, talk, click again — the text is on your clipboard.

- **Mic button**: grey = idle · red (pulsing) = listening · amber = transcribing · **green = done, copied to clipboard**
- **Speaker button** (top-left): toggle **desktop audio capture** for calls (muted speaker = mic only, lit speaker = mic + PC sound)
- **Gear button**: choose where models are stored, pick the Whisper model, set the language, or **delete everything**
- Runs [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on your NVIDIA GPU (CUDA) with the model kept resident in VRAM, so dictation takes a fraction of a second. Falls back to CPU automatically on machines without an NVIDIA GPU.
- **No admin rights, no installer.** Download `SimpleSTT.exe`, run it. Everything it installs lives in `%LOCALAPPDATA%\SimpleSTT` (engine, models, CUDA cache, settings) unless you point the models folder elsewhere.
- **Self-updating**: on launch it checks this repo's latest release, verifies the SHA-256, swaps itself and restarts.

## Install

1. Grab `SimpleSTT.exe` from the [latest release](https://github.com/JenVA9/SimpleSTT/releases/latest).
2. Run it. On first launch it downloads the speech engine (SHA-256 pinned; ~640 MB for the GPU build, ~8 MB for CPU) and the default model (`large-v3-turbo-q5_0`, ~574 MB). The ring around the mic shows progress.
3. The very first GPU start on a brand-new GPU generation (e.g. RTX 50-series) spends ~45 s JIT-compiling CUDA kernels ("Warming up GPU…"). It is cached afterwards and happens in the background.

Space also toggles recording while the window is focused.

## Live transcription (long recordings finish instantly)

While you record, SimpleSTT quietly transcribes each finished phrase in the background (it detects pauses, ~0.7 s of quiet) and appends the text to a temp file, `%LOCALAPPDATA%\SimpleSTT\live-transcript.txt`. Nothing is shown or pasted live. When you press stop only the last unsent phrase is left, so the text hits your clipboard in well under a second whether you recorded 10 seconds or an hour (the temp file is deleted once it's copied). While recording the status shows a running timer. Continuous sound with no pauses is cut at the quietest spot before 20 s.

Measured on an RTX 5080 with `medium.en`, 153 s of speech: stop-to-clipboard **4.3 s before → 0.07 s now**, same accuracy (see the `--ignored` tests in `src/live.rs` for the offline harness).

## Capturing calls (desktop audio)

Click the **speaker icon** in the top-left of the window (it lights up blue when on). While recording, SimpleSTT then mixes whatever your PC is playing (WASAPI loopback of the default output device — the other side of a call, a video, a stream) with your microphone, so one click transcribes both sides. It takes effect on the next recording (no engine restart), and the status line shows "Listening to mic + desktop…". Use headphones in calls, otherwise your mic re-captures the speakers and you get echo. If the output device can't be captured it falls back to mic-only and says so.

## IPA mode (phone recognition)

Pick **powsm-ipa** in Settings → Model to get the International Phonetic Alphabet instead of words, e.g. `əndsoʊmaɪfɛloʊəmɛɹɪkənz…` ([POWSM](https://huggingface.co/espnet/powsm), CMU's Phonetic Open Whisper-style Speech Model, CC-BY-4.0). It works with the live chunking, the desktop-audio toggle and the clipboard exactly like normal dictation (the model sees fixed 20 s windows, ~0.9 s each on an RTX 5080). Output has no spaces between words — that's how the model emits phones. The language setting accepts ISO 639-3 or common 2-letter codes; `auto` is language-independent.

POWSM is a PyTorch/ESPnet model that whisper.cpp can't run, so it needs a separate **Python + PyTorch + ESPnet runtime (~4.2 GB with CUDA, ~0.8 GB CPU-only)** plus the 1.3 GB model. **None of this is downloaded unless you pick the IPA model and click Yes in a warning dialog that lists the sizes and locations.** Nothing about it happens at startup, and editing the config by hand can't trigger it either. The runtime is installed with a pinned copy of [uv](https://github.com/astral-sh/uv) into `%LOCALAPPDATA%\SimpleSTT\ipa` (private Python, no system changes, no admin) and the model files (SHA-256 verified, pinned commit) go to `<models folder>\powsm`. **Delete all data…** removes both.
## Uninstall

Settings → **Delete all data…** removes the engine, all models it downloaded, the CUDA cache and settings (in a custom models folder it only removes the `ggml-*.bin` files it created). Then delete `SimpleSTT.exe`.

## Models

`tiny.en`, `base.en`, `small.en`, `medium.en`, `tiny`, `base`, `small`, `medium`, `large-v3-turbo-q5_0` (default), `large-v3-turbo`, `large-v3`, and `powsm-ipa` (IPA phones, see above) — downloaded from [ggerganov/whisper.cpp on Hugging Face](https://huggingface.co/ggerganov/whisper.cpp) the first time they're selected (resumable).

## How it's built small and fast

SimpleSTT is only glue (UI, mic capture, clipboard, downloads); inference is native whisper.cpp on the GPU. So the Rust side is tuned for size and zero overhead:

- Raw Win32 + GDI+ via `windows-sys` — no UI framework, no HTTP/TLS/JSON crates (WinHTTP, `BCrypt`, `waveIn` and the clipboard come from the OS)
- `opt-level="z"`, fat LTO, `codegen-units=1`, `panic="abort"`, `strip`
- Static CRT (no VC++ redistributable), linker `/OPT:REF,ICF`
- Release builds use nightly `build-std` with `optimize_for_size`, `-Zlocation-detail=none`, `-Zfmt-debug=none` (~450 KB → ~310 KB)
- Desktop audio is a hand-rolled WASAPI loopback (downmix + decimate to 16 kHz, placed on a wall-clock timeline so silent gaps stay aligned with the mic)
- Mic is captured directly as 16 kHz mono PCM (Windows does the resampling); the Whisper server keeps the model loaded and the GPU warm

## Build

```powershell
cargo build --release                 # stable, ~446 KB
./build-release.ps1                   # nightly, smallest (~305 KB) -> dist\SimpleSTT.exe
```

Release: bump `version` in `Cargo.toml`, then `git tag vX.Y.Z && git push --tags`; GitHub Actions builds and publishes the exe, and installed copies update themselves.

## Icon

The app icon is drawn by `tools/make_icon.py` (Pillow) into `assets/icon.res`, which build.rs links into the exe, so builds need neither Python nor rc.exe. The window version label (bottom) is read from `Cargo.toml`.

## License

MIT
