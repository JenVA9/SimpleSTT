# SimpleSTT

A tiny (~300 KB) Windows speech-to-text widget. Click the mic, talk, click again — the text is on your clipboard.

- **Mic button**: grey = idle · red (pulsing) = listening · amber = transcribing · **green = done, copied to clipboard**
- **Gear button**: choose where models are stored, pick the Whisper model, set the language, turn on **desktop audio capture**, or **delete everything**
- Runs [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on your NVIDIA GPU (CUDA) with the model kept resident in VRAM, so dictation takes a fraction of a second. Falls back to CPU automatically on machines without an NVIDIA GPU.
- **No admin rights, no installer.** Download `SimpleSTT.exe`, run it. Everything it installs lives in `%LOCALAPPDATA%\SimpleSTT` (engine, models, CUDA cache, settings) unless you point the models folder elsewhere.
- **Self-updating**: on launch it checks this repo's latest release, verifies the SHA-256, swaps itself and restarts.

## Install

1. Grab `SimpleSTT.exe` from the [latest release](https://github.com/JenVA9/SimpleSTT/releases/latest).
2. Run it. On first launch it downloads the speech engine (SHA-256 pinned; ~640 MB for the GPU build, ~8 MB for CPU) and the default model (`large-v3-turbo-q5_0`, ~574 MB). The ring around the mic shows progress.
3. The very first GPU start on a brand-new GPU generation (e.g. RTX 50-series) spends ~45 s JIT-compiling CUDA kernels ("Warming up GPU…"). It is cached afterwards and happens in the background.

Space also toggles recording while the window is focused.

## Capturing calls (desktop audio)

Settings → **Also capture desktop audio**. While recording, SimpleSTT then mixes whatever your PC is playing (WASAPI loopback of the default output device — the other side of a call, a video, a stream) with your microphone, so one click transcribes both sides. It takes effect on the next recording (no engine restart), and the status line shows "Listening to mic + desktop…". Use headphones in calls, otherwise your mic re-captures the speakers and you get echo. If the output device can't be captured it falls back to mic-only and says so.

## Uninstall

Settings → **Delete all data…** removes the engine, all models it downloaded, the CUDA cache and settings (in a custom models folder it only removes the `ggml-*.bin` files it created). Then delete `SimpleSTT.exe`.

## Models

`tiny.en`, `base.en`, `small.en`, `medium.en`, `tiny`, `base`, `small`, `medium`, `large-v3-turbo-q5_0` (default), `large-v3-turbo`, `large-v3` — downloaded from [ggerganov/whisper.cpp on Hugging Face](https://huggingface.co/ggerganov/whisper.cpp) the first time they're selected (resumable).

## How it's built small and fast

SimpleSTT is only glue (UI, mic capture, clipboard, downloads); inference is native whisper.cpp on the GPU. So the Rust side is tuned for size and zero overhead:

- Raw Win32 + GDI+ via `windows-sys` — no UI framework, no HTTP/TLS/JSON crates (WinHTTP, `BCrypt`, `waveIn` and the clipboard come from the OS)
- `opt-level="z"`, fat LTO, `codegen-units=1`, `panic="abort"`, `strip`
- Static CRT (no VC++ redistributable), linker `/OPT:REF,ICF`
- Release builds use nightly `build-std` with `optimize_for_size`, `-Zlocation-detail=none`, `-Zfmt-debug=none` (~446 KB → ~305 KB)
- Desktop audio is a hand-rolled WASAPI loopback (downmix + decimate to 16 kHz, placed on a wall-clock timeline so silent gaps stay aligned with the mic)
- Mic is captured directly as 16 kHz mono PCM (Windows does the resampling); the Whisper server keeps the model loaded and the GPU warm

## Build

```powershell
cargo build --release                 # stable, ~446 KB
./build-release.ps1                   # nightly, smallest (~305 KB) -> dist\SimpleSTT.exe
```

Release: bump `version` in `Cargo.toml`, then `git tag vX.Y.Z && git push --tags`; GitHub Actions builds and publishes the exe, and installed copies update themselves.

## License

MIT
