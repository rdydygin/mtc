//! Lets GPUI's image loader fetch server images such as avatars.
//!
//! Mattermost serves profile pictures and thumbnails only to authenticated
//! requests. Installing this client as the app's HTTP client means `img()`
//! and `Avatar::src()` can take plain server URLs and still benefit from
//! GPUI's asset cache and off-thread decoding. The token is attached only
//! for the signed-in server (see [`mattermost::Client::fetch`]).
//!
//! GPUI's cache lasts one run. Server images that never change are also
//! kept on disk (see [`crate::cache`]), so they show at once after a
//! restart. Requests arrive here on GPUI's background threads, where
//! reading the disk is fine.

use std::sync::Arc;

use futures::future::BoxFuture;
use gpui_kit::http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use mattermost::Client;

use crate::cache;

pub struct ServerHttpClient {
    client: Client,
    /// The sign-in this client serves, so its downloads aren't saved after
    /// a sign-out clears the cache.
    generation: u64,
}

impl ServerHttpClient {
    pub fn new(client: Client) -> Arc<Self> {
        Arc::new(Self {
            client,
            generation: cache::generation(),
        })
    }
}

impl HttpClient for ServerHttpClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        request: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let client = self.client.clone();
        let uri = request.uri().to_string();
        let keep = never_changes(&client, &uri);
        let generation = self.generation;
        Box::pin(async move {
            if keep && let Some(body) = cache::image(&uri) {
                return Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))?);
            }
            let fetched = client.fetch(&uri).await?;
            if keep && fetched.status == 200 && !fetched.body.is_empty() {
                cache::save_image(generation, &uri, &fetched.body);
            }
            let mut response = Response::builder().status(fetched.status);
            if let Some(content_type) = fetched.content_type {
                response = response.header("content-type", content_type);
            }
            Ok(response.body(AsyncBody::from(fetched.body))?)
        })
    }
}

/// The URL of a user's profile picture. `version` changes when the user
/// uploads a new picture, so the image cache never serves a stale one.
pub fn avatar_url(client: &Client, user: &str, version: i64) -> String {
    format!("{}api/v4/users/{user}/image?_={version}", client.server())
}

/// Whether `url` is a server image whose content never changes, and so may
/// be kept on disk: an uploaded file or a custom emoji, which get a new ID
/// when replaced, or a profile picture at a given version.
fn never_changes(client: &Client, url: &str) -> bool {
    let Some(path) = url
        .strip_prefix(client.server().as_str())
        .and_then(|rest| rest.strip_prefix("api/v4/"))
    else {
        return false;
    };
    path.starts_with("files/")
        || path.starts_with("emoji/")
        || (path.starts_with("users/") && path.contains("/image?_="))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_server_images_that_never_change_are_kept() {
        let client = Client::new_unverified("https://chat.example.com", "token").unwrap();
        let server = "https://chat.example.com/api/v4";
        assert!(never_changes(&client, &avatar_url(&client, "u1", 7)));
        assert!(never_changes(
            &client,
            &format!("{server}/files/f1/preview")
        ));
        assert!(never_changes(&client, &format!("{server}/files/f1")));
        assert!(never_changes(
            &client,
            &client.custom_emoji_url(&"e1".into())
        ));

        assert!(!never_changes(&client, &format!("{server}/users/u1/image")));
        assert!(!never_changes(&client, &format!("{server}/users/me")));
        assert!(!never_changes(
            &client,
            "https://example.com/api/v4/files/f1"
        ));
        assert!(!never_changes(
            &client,
            "https://chat.example.com.evil/api/v4/files/f1"
        ));
    }
}
