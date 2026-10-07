pub mod access;
#[cfg(feature = "store")]
mod basic;
#[cfg(all(test, not(target_arch = "wasm32"), feature = "basic-backend"))]
mod basic_tests;
#[cfg(all(
    any(test, feature = "contract-tests"),
    not(target_arch = "wasm32"),
    feature = "basic-backend"
))]
pub mod contract_tests;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub mod egress;
mod environment;
mod error;
pub use environment::EnvironmentDefinition;
mod credential_policy;
pub mod harness;
mod http;
#[cfg(feature = "http-client")]
mod http_client;
#[cfg(all(test, not(target_arch = "wasm32"), feature = "basic-backend"))]
mod http_tests;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod local_volume;
pub mod protocol;
pub mod resources;
#[cfg(feature = "store")]
mod sandbox;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod sandbox_process;
mod sandbox_provider;
#[cfg(feature = "store")]
mod secrets;
#[cfg(feature = "store")]
pub mod server;
#[cfg(feature = "store")]
mod storage;
#[cfg(all(
    any(test, feature = "contract-tests"),
    not(target_arch = "wasm32"),
    feature = "basic-backend"
))]
pub mod test_support;
pub mod turn_coordinator;
mod types;
mod uuid7;
pub use credential_policy::{CredentialDestination, CredentialPolicy};
pub mod vault;

#[cfg(feature = "store")]
pub use basic::*;
pub use error::*;
pub use http::*;
#[cfg(feature = "http-client")]
pub use http_client::{AccessTokenProvider, HttpClient, HttpResponseError};
#[cfg(feature = "store")]
pub use sandbox::*;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use sandbox_process::with_process_management;
pub use sandbox_provider::*;
pub use types::*;
pub use uuid7::*;

#[cfg(feature = "store")]
pub use storage::Storage;

#[cfg(feature = "store")]
pub mod egress_credentials;

pub mod runtime_host;
