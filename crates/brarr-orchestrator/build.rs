//! Build script: compile the gRPC proto definitions using a vendored
//! `protoc` so contributors do not need to install protobuf system-wide.
//!
//! Pass the vendored protoc path **programmatically** through
//! `prost_build::Config::protoc_executable` rather than via the `PROTOC`
//! environment variable, so we don't need `unsafe { std::env::set_var(..) }`
//! (Rust 2024 marks `set_var` as unsafe; the workspace forbids `unsafe`).
//!
//! The generated Rust code lands in `OUT_DIR` and is included by
//! `src/grpc/proto.rs` via `tonic_prost::include_proto!("brarr.v1")`.

#![allow(
    clippy::expect_used,
    reason = "build script panics are correct: a missing protoc or codegen failure must abort the build loudly"
)]

use std::path::PathBuf;

fn main() {
    let proto_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let proto_file = proto_dir.join("brarr.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored should expose a binary for this platform");

    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(&protoc);

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .compile_with_config(config, &[proto_file], &[proto_dir])
        .expect("compiling brarr.proto must succeed");

    println!("cargo:rerun-if-changed=proto/brarr.proto");
    println!("cargo:rerun-if-changed=build.rs");

    // Tailwind v4 source + every template / Rust file it scans for
    // utility classes. We don't run the CSS compile here (would force
    // a Node-less toolchain on every contributor); instead we emit a
    // warning if the bundled artifact is missing so the local dev
    // notices on the very first run rather than at first HTTP hit.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    println!("cargo:rerun-if-changed=styles/input.css");
    println!("cargo:rerun-if-changed=templates");
    println!("cargo:rerun-if-changed=static/theme.js");
    println!("cargo:rerun-if-changed=static/nav.js");
    println!("cargo:rerun-if-changed=static/modal.js");
    println!("cargo:rerun-if-changed=static/datetime.js");

    let app_css = manifest.join("static").join("app.css");
    if !app_css.exists() {
        println!(
            "cargo:warning=static/app.css missing — run scripts/build-css.{{sh,ps1}} \
             after scripts/install-tailwind.{{sh,ps1}}. The orchestrator will \
             still build, but the admin UI will render unstyled."
        );
    }
}
