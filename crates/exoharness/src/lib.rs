#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod basic;
#[cfg(all(test, not(target_arch = "wasm32"), feature = "basic-backend"))]
mod basic_tests;
#[cfg(all(
    any(test, feature = "contract-tests"),
    not(target_arch = "wasm32"),
    feature = "basic-backend"
))]
pub mod contract_tests;
#[cfg(all(not(target_arch = "wasm32"), feature = "egress"))]
pub mod egress;
mod error;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod http;
#[cfg(all(test, not(target_arch = "wasm32"), feature = "basic-backend"))]
mod http_tests;
pub mod protocol;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod sandbox;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod sandbox_process;
mod sandbox_provider;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod secrets;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub mod server;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
mod storage;
#[cfg(all(test, not(target_arch = "wasm32"), feature = "basic-backend"))]
mod test_support;
mod types;
mod uuid7;

#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use basic::*;
pub use error::*;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use http::*;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use sandbox::*;
#[cfg(all(not(target_arch = "wasm32"), feature = "basic-backend"))]
pub use sandbox_process::with_process_management;
pub use sandbox_provider::*;
pub use types::*;
pub use uuid7::*;
