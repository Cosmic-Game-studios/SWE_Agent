//! Locate the built web UI so it can be embedded into the binary.
//!
//! The UI lives in `ui/` and is built with `npm run build` into `ui/dist`.
//! When that directory does not exist (a plain `cargo build` on a fresh
//! checkout), a placeholder page is embedded instead so the crate still
//! compiles and the server explains what is missing.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let dist = manifest.join("../../ui/dist");
    println!("cargo:rerun-if-changed={}", dist.display());
    println!("cargo:rerun-if-env-changed=SWE_UI_DIR");

    let dir = if let Ok(custom) = std::env::var("SWE_UI_DIR") {
        PathBuf::from(custom)
    } else if dist.join("index.html").is_file() {
        dist.canonicalize().unwrap()
    } else {
        let placeholder = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ui-placeholder");
        std::fs::create_dir_all(&placeholder).unwrap();
        std::fs::write(
            placeholder.join("index.html"),
            "<!doctype html><meta charset=\"utf-8\"><title>SWE Agent</title>\
             <body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto;line-height:1.5\">\
             <h1>SWE Agent</h1><p>The server is running, but this binary was built without the web UI.</p>\
             <p>Build it with <code>npm --prefix ui ci &amp;&amp; npm --prefix ui run build</code> and \
             rebuild the binary. The HTTP API under <code>/api</code> works regardless.</p></body>",
        )
        .unwrap();
        placeholder
    };
    println!("cargo:rustc-env=SWE_UI_DIR={}", dir.display());
}
