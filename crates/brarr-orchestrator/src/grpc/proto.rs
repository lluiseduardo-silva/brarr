//! Re-exports the tonic-generated types for the `brarr.v1` package.
//!
//! The actual codegen lives in `OUT_DIR` and is produced by `build.rs`.
//! Keeping this module thin makes it obvious where the proto surface
//! begins: `crate::grpc::proto::Brarr`, etc.

#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    missing_docs,
    reason = "generated code"
)]

tonic::include_proto!("brarr.v1");
