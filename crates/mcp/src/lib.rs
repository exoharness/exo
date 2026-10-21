mod config;
pub use config::*;

mod client;
pub use client::*;
pub use rmcp::model::{CallToolResult, Tool};

#[cfg(feature = "client")]
mod native;
#[cfg(feature = "client")]
pub use native::{NativeMcpConnector, probe_auth_challenge};

#[cfg(feature = "client")]
mod auth;
