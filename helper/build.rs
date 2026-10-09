// Regenerates the C header for the app-facing ABI in `ffi.rs` whenever the
// crate is built. The header is consumed by app/src/teslaclient.cpp (via the
// include path wired in app/harbour-electric-eel.pro). cbindgen runs on the
// host (it's parse-only); only the resulting header is used by the target
// cross-compile.
fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
    let out = std::path::Path::new(&crate_dir).join("electriceelcore.h");
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(cbindgen::Config {
            language: cbindgen::Language::C,
            export: cbindgen::ExportConfig {
                exclude: vec![
                    "main".into(),
                    "app_main".into(),
                    "electric_eel_ui_prepare".into(),
                    "electric_eel_ui_run".into(),
                    "electric_eel_ui_cleanup".into(),
                ],
                ..Default::default()
            },
            ..Default::default()
        })
        .generate()
        .expect("cbindgen should generate the union header")
        .write_to_file(out);
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/runtime.rs");
}
