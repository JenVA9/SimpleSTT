# Smallest possible SimpleSTT.exe (~300 KB). Needs nightly + rust-src:
#   rustup toolchain install nightly --profile minimal -c rust-src
# Plain `cargo build --release` on stable also works (~440 KB).
$ErrorActionPreference = 'Stop'
$env:RUSTFLAGS = '-C target-feature=+crt-static -C link-arg=/OPT:REF,ICF,LBR -C link-arg=/DEBUG:NONE -Zlocation-detail=none -Zfmt-debug=none'
cargo +nightly build --release `
    -Z build-std=std,panic_abort `
    -Z build-std-features=optimize_for_size `
    --target x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\x86_64-pc-windows-msvc\release\SimpleSTT.exe dist\SimpleSTT.exe -Force
"{0:N0} bytes -> dist\SimpleSTT.exe" -f (Get-Item dist\SimpleSTT.exe).Length
