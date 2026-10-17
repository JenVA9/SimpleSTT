# SimpleSTT

A tiny (~390 KB) Windows speech-to-text widget. Click the mic, talk, click again — the text is on your clipboard.

- **Mic button**: grey = idle · red (pulsing) = listening · amber = transcribing · **green = done, copied to clipboard**
- **Speaker button** (top-left): toggle **desktop audio capture** for calls (muted speaker = mic only, lit speaker = mic + PC sound)
- **Gear button**: model folders, models (incl. your own files), language, IPA/AI options, or **delete everything**
- Runs [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on your NVIDIA GPU (CUDA) with the model kept resident in VRAM, so dictation takes a fraction of a second. Falls back to CPU automatically on machines without an NVIDIA GPU.
- **No admin rights, no installer.** Download `SimpleSTT.exe`, run it. Everything it installs lives in `%LOCALAPPDATA%\SimpleSTT` (engines, runtime, caches, settings); models go to the folders you choose.
- **Self-updating**: on launch it checks this repo's latest release, verifies the SHA-256, swaps itself and restarts. Updating never re-runs the first-run wizard.

## Install & first run

1. Grab `SimpleSTT.exe` from the [latest release](https://github.com/JenVA9/SimpleSTT/releases/latest) and run it.
2. A short wizard asks (nothing is downloaded until you finish it, and Cancel quits cleanly):
   1. which folder to keep **Whisper models** in
   2. which **Whisper model** (★ marks the recommendation for your GPU/RAM, or pick *custom* to use your own ggml `.bin`)
   3. *only if you chose the IPA model:* which folder for **text models**
   4. *only if you chose the IPA model:* which **Gemma model** (★ = the largest one that fits in half your VRAM/RAM, max 5 GB loaded)
3. It then downloads the speech engine (SHA-256 pinned; ~640 MB for the GPU build, ~8 MB for CPU) and your model. The ring around the mic shows progress.
4. The very first GPU start on a brand-new GPU generation (e.g. RTX 50-series) spends ~45 s JIT-compiling CUDA kernels ("Warming up GPU…"). It is cached afterwards and happens in the background.

Existing installs (and any copy that just self-updated) skip the wizard. Space toggles recording while the window is focused.

## Live transcription (long recordings finish instantly)

While you record, SimpleSTT quietly transcribes each finished phrase in the background (it detects pauses, ~0.7 s of quiet) and appends the text to a temp file, `%LOCALAPPDATA%\SimpleSTT\live-transcript.txt`. Nothing is shown or pasted live. When you press stop only the last unsent phrase is left, so the text hits your clipboard in well under a second whether you recorded 10 seconds or an hour (the temp file is deleted once it's copied). While recording the status shows a running timer. Continuous sound with no pauses is cut at the quietest spot before 20 s.

Measured on an RTX 5080 with `medium.en`, 153 s of speech: stop-to-clipboard **4.3 s before → 0.07 s now**, same accuracy (see the `--ignored` tests in `src/live.rs` for the offline harness).

## Capturing calls (desktop audio)

Click the **speaker icon** in the top-left of the window (it lights up blue when on). While recording, SimpleSTT then mixes whatever your PC is playing (WASAPI loopback of the default output device — the other side of a call, a video, a stream) with your microphone, so one click transcribes both sides. It takes effect on the next recording (no engine restart), and the status line shows "Listening to mic + desktop…". Use headphones in calls, otherwise your mic re-captures the speakers and you get echo. If the output device can't be captured it falls back to mic-only and says so.

## IPA mode (phone recognition + AI)

Pick **powsm-ipa** as the model to get the International Phonetic Alphabet instead of words ([POWSM](https://huggingface.co/espnet/powsm), CMU's Phonetic Open Whisper-style Speech Model, CC-BY-4.0). Two stages:

1. **POWSM** turns speech into IPA phones (language-independent; the model sees fixed 20 s windows, ~0.9 s each on an RTX 5080). Raw output looks like `əndsoʊmaɪfɛloʊəmɛɹɪkənz…` — no spaces between words.
2. **Gemma 4** (a small local LLM, reasoning **off**) writes those phones out as properly spelled and punctuated text, in whatever language(s) were spoken. Its prompt (`assets/ipa_prompt.txt`) tells it that a recording can switch language mid-sentence, that every part should be written in its own language and script, and that **if part of the speech isn't a language it recognises it must pick the closest language that fits the sounds** instead of leaving IPA. The AI stage can be switched off in settings to get raw IPA.

> ⚠ **IPA + AI is experimental and can hallucinate** — it may invent, change or "fix" words that weren't said. It is designed for multilingual or unreliable speech (several languages, accents, noisy audio). For normal single-language dictation a regular Whisper model is more accurate.

**Which Gemma?** The app lists every Gemma 4 size (E2B, E4B, 12B, 26B-A4B, 31B; Apache-2.0 QAT GGUFs) plus *custom* (any `.gguf`). The ★ recommendation is the largest model whose estimated loaded size fits in **half your VRAM (RAM without an NVIDIA GPU), capped at 5 GB** — on a 16 GB GPU that is Gemma 4 E4B (Q4). Anything bigger is allowed too; it's marked as above the recommended limit. Text models go in their own folder (settings → *Text models folder*).

**Nothing IPA-related is downloaded at startup, ever.** Selecting the IPA model shows a warning dialog listing every component, its size and its destination (Python/PyTorch/ESPnet runtime ~4.2 GB CUDA or ~0.8 GB CPU, POWSM 1.3 GB, llama.cpp text engine ~30 MB, Gemma model 2–4 GB for the small ones), the free disk space needed while installing, and the hallucination warning. Only **Yes** starts anything, and editing the config by hand can't trigger it either. POWSM is a PyTorch/ESPnet model whisper.cpp can't run, so it uses a private Python installed with a pinned copy of [uv](https://github.com/astral-sh/uv) into `%LOCALAPPDATA%\SimpleSTT\ipa` (no system changes, no admin); the text engine is a pinned [llama.cpp](https://github.com/ggml-org/llama.cpp) build (Vulkan, or CPU-only if there's no Vulkan driver). All model files are SHA-256 verified against pinned commits.

## Uninstall

Settings → **Delete all data…** removes the engines, the IPA runtime, the CUDA cache, settings and every model file SimpleSTT itself downloaded (in your own model folders it only removes those exact files, never anything else, and never custom files you picked). Then delete `SimpleSTT.exe`.

## Models

Whisper (from [ggerganov/whisper.cpp on Hugging Face](https://huggingface.co/ggerganov/whisper.cpp), downloaded the first time they're selected, resumable): `tiny.en`, `base.en`, `small.en`, `medium.en`, `tiny`, `base`, `small`, `medium`, `large-v3-turbo-q5_0`, `large-v3-turbo`, `large-v3`, plus `powsm-ipa` (see above) and *custom* (any ggml `.bin`).

## How it's built small and fast

SimpleSTT is only glue (UI, mic capture, clipboard, downloads); inference is native whisper.cpp / llama.cpp on the GPU. So the Rust side is tuned for size and zero overhead:

- Raw Win32 + GDI+ via `windows-sys` — no UI framework, no HTTP/TLS/JSON crates (WinHTTP, `BCrypt`, `waveIn` and the clipboard come from the OS)
- `opt-level="z"`, fat LTO, `codegen-units=1`, `panic="abort"`, `strip`
- Static CRT (no VC++ redistributable), linker `/OPT:REF,ICF`
- Release builds use nightly `build-std` with `optimize_for_size`, `-Zlocation-detail=none`, `-Zfmt-debug=none`
- Desktop audio is a hand-rolled WASAPI loopback (downmix + decimate to 16 kHz, placed on a wall-clock timeline so silent gaps stay aligned with the mic)
- Mic is captured directly as 16 kHz mono PCM (Windows does the resampling); the engines keep their models loaded and the GPU warm

## Build

```powershell
cargo build --release                 # stable
./build-release.ps1                   # nightly, smallest -> dist\SimpleSTT.exe
```

Release: bump `version` in `Cargo.toml`, then `git tag vX.Y.Z && git push --tags`; GitHub Actions builds and publishes the exe, and installed copies update themselves.

## Icon

The app icon is drawn by `tools/make_icon.py` (Pillow) into `assets/icon.res`, which build.rs links into the exe, so builds need neither Python nor rc.exe. The window version label (bottom) is read from `Cargo.toml`.

## License

MIT
