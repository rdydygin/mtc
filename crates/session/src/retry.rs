//! Repeating requests that failed for reasons that may pass.
//!
//! On a slow or flaky link a request can time out or lose its connection
//! even though the next attempt would succeed. The loads that fill the
//! window at startup retry a few times with backoff instead of leaving the
//! window empty until the next reconnect.

use std::{future::Future, time::Duration};

use anyhow::Result;
use gpui_kit::BackgroundExecutor;
use mattermost::ApiError;

/// Attempts in total, including the first.
const ATTEMPTS: u32 = 4;
const FIRST_DELAY: Duration = Duration::from_millis(500);

/// Whether a failed request is worth repeating: the network or the server
/// hiccuped, rather than the server refusing or answering nonsense.
pub(crate) fn is_transient(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<serde_json::Error>().is_some() {
        return false;
    }
    match err.downcast_ref::<ApiError>() {
        Some(ApiError::Unauthorized) => false,
        Some(ApiError::Status(code)) => *code >= 500 || matches!(*code, 408 | 429),
        None => true,
    }
}

/// Run `request` until it succeeds, fails for good, or runs out of attempts.
pub(crate) async fn retry<T, F, Fut>(executor: &BackgroundExecutor, mut request: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut delay = FIRST_DELAY;
    let mut attempt = 1;
    loop {
        match request().await {
            Err(err) if attempt < ATTEMPTS && is_transient(&err) => {
                log::info!("retrying in {delay:?}: {err:#}");
                executor.timer(delay).await;
                delay *= 2;
                attempt += 1;
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::*;

    #[test]
    fn retries_only_what_may_pass() {
        assert!(is_transient(&anyhow!(
            "The server took too long to respond."
        )));
        assert!(is_transient(&anyhow!(ApiError::Status(502))));
        assert!(is_transient(&anyhow!(ApiError::Status(429))));
        assert!(!is_transient(&anyhow!(ApiError::Status(403))));
        assert!(!is_transient(
            &anyhow!(ApiError::Unauthorized).context("Invalid or expired session")
        ));
        let decode = serde_json::from_str::<u8>("x").unwrap_err();
        assert!(!is_transient(
            &anyhow::Error::from(decode).context("unexpected response")
        ));
    }
}
