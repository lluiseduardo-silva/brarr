//! Build script: compile the gRPC `brarr.proto` for *client* use.
//!
//! The proto file lives in the orchestrator crate
//! (`../brarr-orchestrator/proto/brarr.proto`) — single source of truth.
//! Both build scripts (this one + the orchestrator's) consume it; this
//! one emits client bindings, the orchestrator emits server bindings.

#![allow(
    clippy::expect_used,
    clippy::similar_names,
    reason = "build script panics are correct: a missing protoc or codegen failure must abort the build loudly; protoc/proto/protos names are inherent to the proto toolchain"
)]

use std::path::PathBuf;

fn main() {
    let proto_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("brarr-orchestrator")
        .join("proto")
        .join("brarr.proto");
    let proto_dir = proto_path
        .parent()
        .expect("brarr.proto must live in a directory")
        .to_path_buf();

    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored should expose a binary for this platform");

    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(&protoc);

    let protos = std::slice::from_ref(&proto_path);
    let includes = std::slice::from_ref(&proto_dir);
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(false)
        .compile_with_config(config, protos, includes)
        .expect("compiling brarr.proto must succeed");

    println!("cargo:rerun-if-changed={}", proto_path.to_string_lossy());
    println!("cargo:rerun-if-changed=build.rs");
}
