//! An executor-agnostic Mattermost v4 client.
//!
//! [`Client`] wraps the REST API; [`EventStream`] keeps the real-time
//! WebSocket alive. Both run their I/O on a private tokio runtime, so their
//! futures and channels work from any executor, including GPUI's.

mod client;
mod events;
pub mod runtime;
pub mod sso;
pub mod types;

pub use client::{ApiError, Client, FetchedResource, normalize_server_url};
pub use events::{Event, EventStream};
pub use types::*;
