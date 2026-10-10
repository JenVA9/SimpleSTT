// Embeds a manifest: Common Controls v6 (modern look) and asInvoker (never asks for admin).
fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // App icon: pre-built resource (regenerate with tools/make_icon.py); no rc.exe needed.
    println!("cargo:rerun-if-changed=assets/icon.res");
    println!("cargo:rustc-link-arg-bins=assets/icon.res");
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:level='asInvoker' uiAccess='false'");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTDEPENDENCY:type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
    );
}
