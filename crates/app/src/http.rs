//! Lets GPUI's image loader fetch server images such as avatars.
//!
//! Mattermost serves profile pictures and thumbnails only to authenticated
//! requests. Installing this client as the app's HTTP client means `img()`
//! and `Avatar::src()` can take plain server URLs and still benefit from
//! GPUI's asset cache and off-thread decoding. The token is attached only
//! for the signed-in server (see [`mattermost::Client::fetch`]).

use std::sync::Arc;

use futures::future::BoxFuture;
use gpui_kit::http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use mattermost::Client;

pub struct ServerHttpClient {
    client: Client,
}

impl ServerHttpClient {
    pub fn new(client: Client) -> Arc<Self> {
        Arc::new(Self { client })
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
        Box::pin(async move {
            let fetched = client.fetch(&uri).await?;
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
