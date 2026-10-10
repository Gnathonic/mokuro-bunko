//! The mokuro-bunko library server.

// Handlers return early with `Result<_, Response>` (an axum `Response` is ~128 bytes);
// boxing every early reply would only add noise.
#![allow(clippy::result_large_err)]

pub mod accounts;
pub mod admin;
pub mod app;
pub mod auth;
pub mod backend;
pub mod core;
pub mod davhooks;
pub mod glue;
pub mod http;
pub mod library;
pub mod machine;
pub mod ocr;
pub mod ops;
pub mod serve;
pub mod thumbs;
pub mod tls;

pub use core::{Core, RequestCtx};
