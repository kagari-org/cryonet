#[cfg(not(target_arch = "wasm32"))]
pub use tokio::time::{Instant, Interval, interval};
#[cfg(target_arch = "wasm32")]
pub use wasmtimer::{
    std::Instant,
    tokio::{Interval, interval},
};
