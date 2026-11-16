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
//!
//! Every picture GPUI loads by URL comes through here, so this is where an
//! SVG body is handed to a helper process and GPUI gets only its raster
//! image (see [`crate::svg_process`]).

use std::sync::Arc;

use futures::future::BoxFuture;
use gpui_kit::http_client::{AsyncBody, HttpClient, Request, Response, Url, http::HeaderValue};
use mattermost::Client;

use crate::{cache, image_guard::is_safe_image, svg_process};

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
                if !is_safe_image(&body) {
                    return refused();
                }
                let Some(body) = renderable(body).await else {
                    return refused();
                };
                return Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))?);
            }
            let fetched = client.fetch(&uri).await?;
            if !is_safe_image(&fetched.body) {
                log::warn!(
                    "not showing an SVG image that reads local files, nests compressed SVG, or is too costly to parse"
                );
                return refused();
            }
            if keep && fetched.status == 200 && !fetched.body.is_empty() {
                cache::save_image(generation, &uri, &fetched.body);
            }
            // GPUI reads the body of a successful response only.
            let body = if (200..300).contains(&fetched.status) {
                match renderable(fetched.body).await {
                    Some(body) => body,
                    None => return refused(),
                }
            } else {
                fetched.body
            };
            let mut response = Response::builder().status(fetched.status);
            if let Some(content_type) = fetched.content_type {
                response = response.header("content-type", content_type);
            }
            Ok(response.body(AsyncBody::from(body))?)
        })
    }
}

/// `body`, which [`is_safe_image`] allows, as GPUI may draw it: a raster
/// image untouched, an SVG rendered by a helper process. `None` if the
/// helper refused it or couldn't render it in time.
async fn renderable(body: Vec<u8>) -> Option<Vec<u8>> {
    if !svg_process::needs_helper(&body) {
        return Some(body);
    }
    let raster = svg_process::helpers()?.rasterize(body).await;
    if raster.is_none() {
        log::warn!("not showing an SVG image its helper refused or couldn't render in time");
    }
    raster
}

/// A failed response, which GPUI shows as a picture that can't be shown.
fn refused() -> anyhow::Result<Response<AsyncBody>> {
    Ok(Response::builder()
        .status(415)
        .body(AsyncBody::from(Vec::new()))?)
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

/// Answers every request with a small, valid PNG, so tests draw real
/// pictures without a network and the test executor drives each load.
#[cfg(test)]
pub(crate) struct PictureClient;

#[cfg(test)]
impl PictureClient {
    /// A 2 × 2 red PNG.
    const PNG: [u8; 74] = [
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 2, 0, 0, 0, 2, 8, 6,
        0, 0, 0, 114, 182, 13, 36, 0, 0, 0, 17, 73, 68, 65, 84, 120, 156, 99, 248, 207, 192, 240,
        31, 132, 25, 96, 12, 0, 71, 202, 7, 249, 103, 89, 110, 183, 0, 0, 0, 0, 73, 69, 78, 68,
        174, 66, 96, 130,
    ];

    pub(crate) fn install(cx: &mut gpui_kit::App) {
        cx.set_http_client(Arc::new(Self));
    }
}

#[cfg(test)]
impl HttpClient for PictureClient {
    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        _: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        Box::pin(async {
            Ok(Response::builder()
                .status(200)
                .header("content-type", "image/png")
                .body(AsyncBody::from(Self::PNG.to_vec()))?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_images_reach_gpui_untouched() {
        let png = PictureClient::PNG.to_vec();
        assert!(!svg_process::needs_helper(&png));
        let body = futures::executor::block_on(renderable(png.clone()));
        assert_eq!(body, Some(png));
    }

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
