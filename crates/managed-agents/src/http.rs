#[cfg(feature = "client")]
mod client;
pub mod protocol;
pub mod sse;

#[cfg(feature = "client")]
pub use client::RuntimeClient;

pub const RUNTIME_PATH: &str = "/exo";
