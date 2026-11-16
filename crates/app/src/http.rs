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

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
                if !is_safe_image(&body) {
                    return refused();
                }
                return Ok(Response::builder()
                    .status(200)
                    .body(AsyncBody::from(body))?);
            }
            let fetched = client.fetch(&uri).await?;
            if !is_safe_image(&fetched.body) {
                log::warn!(
                    "not showing an SVG image that reads local files or nests compressed SVG"
                );
                return refused();
            }
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

/// A failed response, which GPUI shows as a picture that can't be shown.
fn refused() -> anyhow::Result<Response<AsyncBody>> {
    Ok(Response::builder()
        .status(415)
        .body(AsyncBody::from(Vec::new()))?)
}

/// Whether GPUI may render `body`.
///
/// GPUI decodes the raster formats it recognizes and renders everything
/// else as SVG, with usvg's default options. Those resolve an `<image>` or
/// `<feImage>` link as a local path and read the whole file: `/dev/zero`
/// exhausts memory, a FIFO hangs the loader, and on Windows a UNC path
/// connects to another host. Compressed SVG, at the top level or nested in
/// a `data:` link, is inflated without a limit. Any message or attachment
/// can carry such an image, so those SVGs aren't shown. SVGs that embed
/// raster images as `data:` links still are.
fn is_safe_image(body: &[u8]) -> bool {
    const GZIP: [u8; 2] = [0x1f, 0x8b];
    if body.starts_with(&GZIP) {
        return false;
    }
    let unsafe_link = Arc::new(AtomicBool::new(false));
    let (on_string, on_data) = (unsafe_link.clone(), unsafe_link.clone());
    let options = usvg::Options {
        image_href_resolver: usvg::ImageHrefResolver {
            resolve_string: Box::new(move |_, _| {
                on_string.store(true, Ordering::Relaxed);
                None
            }),
            resolve_data: Box::new(move |mime, data, _| match mime {
                "image/jpg" | "image/jpeg" => Some(usvg::ImageKind::JPEG(data)),
                "image/png" => Some(usvg::ImageKind::PNG(data)),
                "image/gif" => Some(usvg::ImageKind::GIF(data)),
                "image/webp" => Some(usvg::ImageKind::WEBP(data)),
                // usvg's own resolver parses SVG here, or sniffs plain text
                // as SVG, inflating it if compressed.
                "image/svg+xml" | "text/plain" => {
                    on_data.store(true, Ordering::Relaxed);
                    None
                }
                _ => None,
            }),
        },
        ..Default::default()
    };
    // Raster images aren't UTF-8 text and fail at once; an SVG that fails
    // here fails the same way in GPUI, before it resolves any link.
    let _ = usvg::Tree::from_data(body, &options);
    !unsafe_link.load(Ordering::Relaxed)
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
    fn svgs_that_read_files_or_inflate_data_are_refused() {
        let svg = |inner: &str| {
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink" width="10" height="10">{inner}</svg>"#
            )
        };
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
        for (inner, safe) in [
            (r#"<rect width="10" height="10" fill="red"/>"#.to_string(), true),
            (
                format!(r#"<image width="1" height="1" href="data:image/png;base64,{png}"/>"#),
                true,
            ),
            (r#"<image width="1" height="1" href="/dev/zero"/>"#.into(), false),
            (r#"<image width="1" height="1" xlink:href="/dev/zero"/>"#.into(), false),
            (r#"<image width="1" height="1" href="secret.png"/>"#.into(), false),
            (r#"<image width="1" height="1" href="\\host\share\a.png"/>"#.into(), false),
            (
                r#"<filter id="f"><feImage href="/dev/zero"/></filter><rect width="1" height="1" filter="url(#f)"/>"#.into(),
                false,
            ),
            (
                r#"<image width="1" height="1" href="data:image/svg+xml;base64,H4sIAAAAAAAA"/>"#.into(),
                false,
            ),
            (
                r#"<image width="1" height="1" href="data:text/plain;base64,H4sIAAAAAAAA"/>"#.into(),
                false,
            ),
        ] {
            assert_eq!(is_safe_image(svg(&inner).as_bytes()), safe, "{inner}");
        }
        // Compressed SVG.
        assert!(!is_safe_image(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0]));
        // Raster images and other bytes pass through to GPUI's decoders.
        assert!(is_safe_image(&[
            0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'
        ]));
        assert!(is_safe_image(&[0xff, 0xd8, 0xff, 0xe0]));
        assert!(is_safe_image(b"GIF89a"));
        assert!(is_safe_image(b"not an image"));
        assert!(is_safe_image(b""));
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
