//! Tauri's codegen runs only for the app build.
//!
//! `cargo test -p ic_widget` builds the library without the `app` feature and
//! must not pay for (or require) the Tauri toolchain, the frontend bundle, or a
//! `tauri.conf.json` that points at a `ui/dist` nobody built.

fn main() {
    #[cfg(feature = "app")]
    tauri_build::build();

    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-changed=capabilities");
}
