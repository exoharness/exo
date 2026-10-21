mod client;
pub use client::{ExoHttpTransport, HttpExoHarness, HttpSandboxHandle};
mod process;
#[cfg(feature = "basic-backend")]
mod server;
#[cfg(feature = "basic-backend")]
pub use server::{
    ExoHarnessHttpServeOptions, serve_exoharness_http, serve_exoharness_http_listener,
    serve_exoharness_http_listener_with_options, serve_exoharness_http_with_options,
};

pub const HTTP_EXOHARNESS_REQUEST_PATH: &str = "/request";
pub const HTTP_EXOHARNESS_TRACING_TARGET: &str = "exo::exoharness_http";
