#[cfg(not(target_arch = "wasm32"))]
mod loader;
mod table;

#[cfg(not(target_arch = "wasm32"))]
pub use loader::load;
pub use table::{ModelEntry, PricingTable, TokenCounts};
