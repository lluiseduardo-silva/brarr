//! gRPC server module.
//!
//! `proto.rs` re-exports the codegen output (`tonic::include_proto!`),
//! `service.rs` implements the `Brarr` service trait against [`AppState`].

pub mod proto;
pub mod service;

pub use service::serve;
