//! A private tokio runtime that makes the client usable from any executor.
//!
//! `reqwest` and `tokio-tungstenite` need a tokio reactor, while UI toolkits
//! such as GPUI run their own executors. Every public future in this crate
//! hops onto this runtime and resolves through a `JoinHandle`, which can be
//! awaited from any executor. Network I/O and JSON decoding therefore never
//! run on the caller's thread.

use std::{future::Future, sync::LazyLock};

use anyhow::{Context as _, Result};

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("mattermost-io")
        .enable_all()
        .build()
        .expect("failed to start the Mattermost I/O runtime")
});

/// Run `future` on the I/O runtime and await its output from any executor.
pub async fn run<F, T>(future: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    RUNTIME
        .spawn(future)
        .await
        .context("Mattermost I/O task was cancelled")?
}

/// Start `future` on the I/O runtime without awaiting it.
pub fn spawn<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    RUNTIME.spawn(future)
}
