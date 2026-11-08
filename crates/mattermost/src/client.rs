use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context as _, Result, anyhow, bail};
use reqwest::{Method, StatusCode, header};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use url::Url;

use crate::{
    runtime,
    types::{
        Channel, ChannelId, ChannelMember, CustomStatus, Emoji, EmojiId, FileInfo, Post, PostId,
        PostList, Preference, Status, Team, TeamId, User, UserId, UserStatus,
    },
};

/// An authenticated Mattermost v4 client.
///
/// Cloning is cheap: clones share one connection pool and one token. Every
/// request runs on the crate's I/O runtime, so the returned futures can be
/// awaited from any executor without blocking it.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    server: Url,
    token: String,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("server", &self.inner.server.as_str())
            .finish_non_exhaustive()
    }
}

/// Turn what a person types into a server base URL: `chat.example.com`
/// becomes `https://chat.example.com/`.
pub fn normalize_server_url(input: &str) -> Result<Url> {
    let input = input.trim().trim_end_matches('/');
    if input.is_empty() {
        bail!("Enter a server address.");
    }
    let with_scheme = if input.contains("://") {
        input.to_string()
    } else {
        format!("https://{input}")
    };
    let mut url =
        Url::parse(&with_scheme).map_err(|_| anyhow!("“{input}” isn’t a valid address."))?;
    if !matches!(url.scheme(), "http" | "https") {
        bail!("Use an http or https address.");
    }
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
}

/// How long a JSON API request may take in total. Generous, because on a
/// slow link a large channel list can take a while to arrive; a connection
/// that stops sending entirely is caught sooner by [`Timeouts::read`].
const API_TIMEOUT: Duration = Duration::from_secs(120);

/// How long requests may wait. Separate from the client so tests can shrink
/// them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timeouts {
    /// Establishing the TCP and TLS connection. Covers a few round trips on
    /// a high-latency link.
    pub connect: Duration,
    /// Silence between two reads of a response. This is what catches a dead
    /// connection: a slow download that keeps making progress never trips
    /// it, however long it takes in total.
    pub read: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(15),
            read: Duration::from_secs(30),
        }
    }
}

pub(crate) fn http_client(timeouts: Timeouts) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("mtc/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(timeouts.connect)
        // No total timeout here: downloads and uploads are bounded by
        // progress, not by size. JSON requests add `API_TIMEOUT` themselves.
        .read_timeout(timeouts.read)
        .pool_idle_timeout(Duration::from_secs(90))
        // Notice a half-open connection (laptop lid, Wi-Fi change) instead
        // of waiting on it until the read timeout.
        .tcp_keepalive(Duration::from_secs(30))
        // Grow HTTP/2 flow-control windows to the link's bandwidth-delay
        // product, so a high-latency link isn't capped by the default window.
        .http2_adaptive_window(true)
        .build()
        .context("failed to build the HTTP client")
}

impl Client {
    /// Sign in with a username or email and password. `mfa_token` is the
    /// one-time code for accounts with multi-factor authentication.
    pub async fn login(
        server: &str,
        login_id: &str,
        password: &str,
        mfa_token: Option<&str>,
    ) -> Result<(Client, User)> {
        let server = normalize_server_url(server)?;
        let mut body = json!({ "login_id": login_id.trim(), "password": password });
        if let Some(code) = mfa_token.filter(|code| !code.trim().is_empty()) {
            body["token"] = json!(code.trim());
        }
        runtime::run(async move {
            let http = http_client(Timeouts::default())?;
            let url = server.join("api/v4/users/login")?;
            let response = http
                .post(url)
                .timeout(API_TIMEOUT)
                .json(&body)
                .send()
                .await
                .map_err(connect_error)?;
            let response = check(response).await?;
            let token = response
                .headers()
                .get("Token")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| anyhow!("The server didn’t return a session token."))?;
            let user: User = response.json().await?;
            let client = Client {
                inner: Arc::new(Inner {
                    http,
                    server,
                    token,
                }),
            };
            Ok((client, user))
        })
        .await
    }

    /// Resume a session or use a personal access token. The token is checked
    /// by loading the current user.
    pub async fn with_token(server: &str, token: &str) -> Result<(Client, User)> {
        let client = Self::new_unverified(server, token)?;
        let me = client.me().await?;
        Ok((client, me))
    }

    /// A client for `server` and `token` without contacting the server.
    /// Prefer [`Self::with_token`], which verifies the token.
    pub fn new_unverified(server: &str, token: &str) -> Result<Client> {
        Self::with_timeouts(server, token, Timeouts::default())
    }

    pub(crate) fn with_timeouts(server: &str, token: &str, timeouts: Timeouts) -> Result<Client> {
        Ok(Client {
            inner: Arc::new(Inner {
                http: http_client(timeouts)?,
                server: normalize_server_url(server)?,
                token: token.trim().to_string(),
            }),
        })
    }

    pub fn server(&self) -> &Url {
        &self.inner.server
    }

    pub fn token(&self) -> &str {
        &self.inner.token
    }

    pub(crate) fn websocket_url(&self) -> Result<Url> {
        let mut url = self.inner.server.join("api/v4/websocket")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| anyhow!("cannot derive a WebSocket address"))?;
        Ok(url)
    }

    async fn request<T, B>(&self, method: Method, path: String, body: Option<B>) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
        B: Serialize + Send + 'static,
    {
        let inner = self.inner.clone();
        runtime::run(async move {
            let url = inner.server.join(&format!("api/v4/{path}"))?;
            let mut request = inner
                .http
                .request(method, url)
                .bearer_auth(&inner.token)
                .header("X-Requested-With", "XMLHttpRequest")
                .timeout(API_TIMEOUT);
            if let Some(body) = body {
                request = request.json(&body);
            }
            let response = check(request.send().await.map_err(connect_error)?).await?;
            // Decode from bytes so an empty body can stand in for `()`.
            let bytes = response.bytes().await.map_err(connect_error)?;
            let bytes = if bytes.is_empty() {
                &b"null"[..]
            } else {
                &bytes[..]
            };
            serde_json::from_slice(bytes)
                .with_context(|| format!("unexpected response from {path}"))
        })
        .await
    }

    async fn get<T: DeserializeOwned + Send + 'static>(&self, path: String) -> Result<T> {
        self.request::<T, ()>(Method::GET, path, None).await
    }

    async fn post<T, B>(&self, path: String, body: B) -> Result<T>
    where
        T: DeserializeOwned + Send + 'static,
        B: Serialize + Send + 'static,
    {
        self.request(Method::POST, path, Some(body)).await
    }

    pub async fn logout(&self) -> Result<()> {
        self.post::<serde_json::Value, _>("users/logout".into(), json!({}))
            .await
            .map(|_| ())
    }

    pub async fn me(&self) -> Result<User> {
        self.get("users/me".into()).await
    }

    pub async fn my_teams(&self) -> Result<Vec<Team>> {
        self.get("users/me/teams".into()).await
    }

    /// Every channel the user belongs to in `team`, including direct and
    /// group messages.
    pub async fn my_channels(&self, team: &TeamId) -> Result<Vec<Channel>> {
        self.get(format!("users/me/teams/{team}/channels")).await
    }

    /// The user's read state for every channel returned by [`Self::my_channels`].
    pub async fn my_channel_members(&self, team: &TeamId) -> Result<Vec<ChannelMember>> {
        self.get(format!("users/me/teams/{team}/channels/members"))
            .await
    }

    pub async fn channel(&self, channel: &ChannelId) -> Result<Channel> {
        self.get(format!("channels/{channel}")).await
    }

    pub async fn my_channel_member(&self, channel: &ChannelId) -> Result<ChannelMember> {
        self.get(format!("channels/{channel}/members/me")).await
    }

    /// The newest `per_page` posts of a channel.
    pub async fn latest_posts(&self, channel: &ChannelId, per_page: usize) -> Result<PostList> {
        self.get(format!(
            "channels/{channel}/posts?page=0&per_page={per_page}&collapsedThreads=true"
        ))
        .await
    }

    /// Up to `per_page` posts older than `before`.
    pub async fn posts_before(
        &self,
        channel: &ChannelId,
        before: &PostId,
        per_page: usize,
    ) -> Result<PostList> {
        self.get(format!(
            "channels/{channel}/posts?before={before}&per_page={per_page}&collapsedThreads=true"
        ))
        .await
    }

    /// Posts created, edited, or deleted since `since` (milliseconds).
    pub async fn posts_since(&self, channel: &ChannelId, since: i64) -> Result<PostList> {
        self.get(format!(
            "channels/{channel}/posts?since={since}&collapsedThreads=true"
        ))
        .await
    }

    /// A thread's root and every reply.
    pub async fn thread(&self, root: &PostId) -> Result<PostList> {
        self.get(format!("posts/{root}/thread")).await
    }

    pub async fn create_post(
        &self,
        channel: &ChannelId,
        message: &str,
        root: Option<&PostId>,
        pending_post_id: &str,
        file_ids: &[String],
    ) -> Result<Post> {
        let body = json!({
            "channel_id": channel,
            "message": message,
            "root_id": root.map(|r| r.as_str()).unwrap_or(""),
            "pending_post_id": pending_post_id,
            "file_ids": file_ids,
        });
        self.post("posts".into(), body).await
    }

    pub async fn edit_post(&self, post: &PostId, message: &str) -> Result<Post> {
        self.request(
            Method::PUT,
            format!("posts/{post}/patch"),
            Some(json!({ "message": message })),
        )
        .await
    }

    pub async fn delete_post(&self, post: &PostId) -> Result<()> {
        self.request::<serde_json::Value, ()>(Method::DELETE, format!("posts/{post}"), None)
            .await
            .map(|_| ())
    }

    /// Mark `channel` as read.
    pub async fn view_channel(&self, channel: &ChannelId) -> Result<()> {
        self.post::<serde_json::Value, _>(
            "channels/members/me/view".into(),
            json!({ "channel_id": channel }),
        )
        .await
        .map(|_| ())
    }

    pub async fn users_by_ids(&self, ids: Vec<UserId>) -> Result<Vec<User>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.post("users/ids".into(), ids).await
    }

    /// The members of each group message, keyed by channel. The server
    /// names group messages after members' usernames; clients rebuild the
    /// name from these profiles.
    pub async fn group_channel_members(
        &self,
        channels: Vec<ChannelId>,
    ) -> Result<HashMap<ChannelId, Vec<User>>> {
        if channels.is_empty() {
            return Ok(HashMap::new());
        }
        self.post("users/group_channels".into(), channels).await
    }

    pub async fn statuses_by_ids(&self, ids: Vec<UserId>) -> Result<Vec<(UserId, Status)>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let statuses: Vec<UserStatus> = self.post("users/status/ids".into(), ids).await?;
        Ok(statuses
            .into_iter()
            .map(|s| (s.user_id, Status::parse(&s.status)))
            .collect())
    }

    /// Set the user's presence (`Offline` appears offline to others).
    pub async fn set_status(&self, user: &UserId, status: Status) -> Result<()> {
        self.request::<serde_json::Value, _>(
            Method::PUT,
            format!("users/{user}/status"),
            Some(json!({ "user_id": user, "status": status.as_str() })),
        )
        .await
        .map(|_| ())
    }

    pub async fn set_custom_status(&self, status: CustomStatus) -> Result<()> {
        self.request::<serde_json::Value, _>(
            Method::PUT,
            "users/me/status/custom".into(),
            Some(status),
        )
        .await
        .map(|_| ())
    }

    pub async fn clear_custom_status(&self) -> Result<()> {
        self.request::<serde_json::Value, ()>(Method::DELETE, "users/me/status/custom".into(), None)
            .await
            .map(|_| ())
    }

    /// The raw bytes of a user's profile picture.
    pub async fn user_image(&self, user: &UserId) -> Result<Vec<u8>> {
        self.bytes(format!("users/{user}/image")).await
    }

    /// Upload a file to `channel` so a post can attach it by id.
    pub async fn upload_file(
        &self,
        channel: &ChannelId,
        file_name: String,
        bytes: Vec<u8>,
    ) -> Result<FileInfo> {
        #[derive(serde::Deserialize)]
        struct Uploaded {
            file_infos: Vec<FileInfo>,
        }
        let inner = self.inner.clone();
        let channel = channel.to_string();
        runtime::run(async move {
            let part = reqwest::multipart::Part::bytes(bytes).file_name(file_name);
            let form = reqwest::multipart::Form::new()
                .text("channel_id", channel)
                .part("files", part);
            let response = inner
                .http
                .post(inner.server.join("api/v4/files")?)
                .bearer_auth(&inner.token)
                .header("X-Requested-With", "XMLHttpRequest")
                // Sending the body produces no reads, so the read timeout
                // can't tell a slow upload from a stuck one. Bound it in
                // total, generously: large files on slow uplinks take time.
                .timeout(Duration::from_secs(30 * 60))
                .multipart(form)
                .send()
                .await
                .map_err(connect_error)?;
            let uploaded: Uploaded = check(response).await?.json().await?;
            uploaded
                .file_infos
                .into_iter()
                .next()
                .ok_or_else(|| anyhow!("The server didn’t accept the file."))
        })
        .await
    }

    /// The URL of a file's preview image, or the file itself for formats
    /// without one (GIF, SVG). Served to the image cache by `fetch`.
    pub fn file_image_url(&self, file: &FileInfo) -> String {
        if file.has_preview_image {
            format!("{}api/v4/files/{}/preview", self.inner.server, file.id)
        } else {
            format!("{}api/v4/files/{}", self.inner.server, file.id)
        }
    }

    /// The full contents of an uploaded file.
    pub async fn download_file(&self, file_id: &str) -> Result<Vec<u8>> {
        self.bytes(format!("files/{file_id}")).await
    }

    /// The raw bytes of an uploaded file's thumbnail.
    pub async fn file_thumbnail(&self, file_id: &str) -> Result<Vec<u8>> {
        self.bytes(format!("files/{file_id}/thumbnail")).await
    }

    async fn bytes(&self, path: String) -> Result<Vec<u8>> {
        let inner = self.inner.clone();
        runtime::run(async move {
            let url = inner.server.join(&format!("api/v4/{path}"))?;
            let response = inner
                .http
                .get(url)
                .bearer_auth(&inner.token)
                .send()
                .await
                .map_err(connect_error)?;
            Ok(check(response)
                .await?
                .bytes()
                .await
                .map_err(connect_error)?
                .to_vec())
        })
        .await
    }

    /// Fetch an absolute URL. The session token is attached only when the URL
    /// belongs to this client's server, so a link to another host never sees
    /// it. Used to serve images (avatars, thumbnails) to the UI toolkit.
    pub async fn fetch(&self, url: &str) -> Result<FetchedResource> {
        let url = Url::parse(url)?;
        let inner = self.inner.clone();
        runtime::run(async move {
            let same_origin = url.origin() == inner.server.origin();
            let mut request = inner.http.get(url);
            if same_origin {
                request = request.bearer_auth(&inner.token);
            }
            let response = request.send().await.map_err(connect_error)?;
            let status = response.status().as_u16();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let body = response.bytes().await.map_err(connect_error)?.to_vec();
            Ok(FetchedResource {
                status,
                content_type,
                body,
            })
        })
        .await
    }

    pub async fn add_reaction(&self, user: &UserId, post: &PostId, emoji_name: &str) -> Result<()> {
        self.post::<serde_json::Value, _>(
            "reactions".into(),
            json!({ "user_id": user, "post_id": post, "emoji_name": emoji_name }),
        )
        .await
        .map(|_| ())
    }

    pub async fn remove_reaction(
        &self,
        user: &UserId,
        post: &PostId,
        emoji_name: &str,
    ) -> Result<()> {
        self.request::<serde_json::Value, ()>(
            Method::DELETE,
            format!("users/{user}/posts/{post}/reactions/{emoji_name}"),
            None,
        )
        .await
        .map(|_| ())
    }

    /// One page of the server's custom emoji, sorted by name. Fails when
    /// the server has custom emoji turned off.
    pub async fn custom_emoji(&self, page: usize, per_page: usize) -> Result<Vec<Emoji>> {
        self.get(format!("emoji?page={page}&per_page={per_page}&sort=name"))
            .await
    }

    /// The image of a custom emoji. Served to the image cache by `fetch`.
    pub fn custom_emoji_url(&self, emoji: &EmojiId) -> String {
        format!("{}api/v4/emoji/{emoji}/image", self.inner.server)
    }

    /// The user's preferences in `category`, such as [`crate::FAVORITE_CHANNEL`].
    pub async fn my_preferences(&self, category: &str) -> Result<Vec<Preference>> {
        self.get(format!("users/me/preferences/{category}")).await
    }

    /// Create or replace preferences.
    pub async fn save_preferences(
        &self,
        user: &UserId,
        preferences: Vec<Preference>,
    ) -> Result<()> {
        self.request::<serde_json::Value, _>(
            Method::PUT,
            format!("users/{user}/preferences"),
            Some(preferences),
        )
        .await
        .map(|_| ())
    }

    pub async fn delete_preferences(
        &self,
        user: &UserId,
        preferences: Vec<Preference>,
    ) -> Result<()> {
        self.post::<serde_json::Value, _>(format!("users/{user}/preferences/delete"), preferences)
            .await
            .map(|_| ())
    }

    /// Open (or create) the direct message channel between two users.
    pub async fn direct_channel(&self, me: &UserId, other: &UserId) -> Result<Channel> {
        self.post("channels/direct".into(), vec![me.clone(), other.clone()])
            .await
    }

    /// Run a server-side slash command such as `/invite` in `channel`.
    pub async fn execute_command(
        &self,
        channel: &ChannelId,
        team: &TeamId,
        command: &str,
    ) -> Result<()> {
        self.post::<serde_json::Value, _>(
            "commands/execute".into(),
            json!({ "channel_id": channel, "team_id": team, "command": command }),
        )
        .await
        .map(|_| ())
    }

    /// Search users by name for autocomplete and starting conversations.
    pub async fn search_users(&self, term: &str, limit: usize) -> Result<Vec<User>> {
        self.post(
            "users/search".into(),
            json!({ "term": term, "limit": limit, "allow_inactive": false }),
        )
        .await
    }
}

fn connect_error(err: reqwest::Error) -> anyhow::Error {
    if err.is_timeout() {
        anyhow!("The server took too long to respond.")
    } else if err.is_connect() {
        anyhow!("Couldn’t reach the server. Check the address and your connection.")
    } else if err.is_body() || err.is_decode() {
        anyhow!("The connection dropped before the server finished answering.")
    } else {
        anyhow!(err)
    }
}

/// Map an unsuccessful response to the server's own message when it sent one.
async fn check(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    let message = if is_json {
        response
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| {
                v.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
            })
    } else {
        None
    };
    Err(match (status, message) {
        (StatusCode::UNAUTHORIZED, None) => anyhow!(ApiError::Unauthorized),
        (StatusCode::UNAUTHORIZED, Some(message)) => {
            anyhow!(ApiError::Unauthorized).context(message)
        }
        (_, Some(message)) => anyhow!(ApiError::Status(status.as_u16())).context(message),
        (_, None) => anyhow!(ApiError::Status(status.as_u16())),
    })
}

/// The raw result of [`Client::fetch`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FetchedResource {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// Failures callers branch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiError {
    /// The session expired or the token was revoked.
    Unauthorized,
    Status(u16),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("Your session has expired. Sign in again."),
            Self::Status(code) => write!(f, "The server returned an error ({code})."),
        }
    }
}

impl std::error::Error for ApiError {}

#[cfg(test)]
mod tests {
    use std::{io::Write as _, time::Instant};

    use crate::testing::{Network, Reply, SlowServer};

    use super::*;

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        futures::executor::block_on(future)
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    /// A channel list as large as a busy team's, which compresses well.
    fn many_channels() -> serde_json::Value {
        let channels: Vec<_> = (0..400)
            .map(|i| {
                json!({
                    "id": format!("channel{i:022}"),
                    "team_id": "team",
                    "type": "O",
                    "display_name": format!("Channel number {i}"),
                    "name": format!("channel-{i}"),
                    "header": "Discussion about the project and everything related to it",
                    "purpose": "A place for the team to talk",
                })
            })
            .collect();
        json!(channels)
    }

    #[test]
    fn asks_for_and_decodes_compressed_responses() {
        let body = many_channels().to_string();
        let compressed = gzip(body.as_bytes());
        // Compression is what makes large lists bearable on a slow link.
        assert!(compressed.len() * 5 < body.len());
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            if request
                .header("accept-encoding")
                .is_some_and(|v| v.contains("gzip"))
            {
                Reply::bytes(compressed.clone())
                    .header("Content-Type", "application/json")
                    .header("Content-Encoding", "gzip")
            } else {
                Reply::json(json!({"message": "send Accept-Encoding"})).status(400)
            }
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let channels = block_on(client.my_channels(&TeamId::from("team"))).unwrap();
        assert_eq!(channels.len(), 400);
        let accept = server.requests()[0]
            .header("accept-encoding")
            .unwrap_or_default()
            .to_string();
        for encoding in ["gzip", "br", "zstd", "deflate"] {
            assert!(accept.contains(encoding), "Accept-Encoding: {accept}");
        }
    }

    #[test]
    fn slow_downloads_finish_however_long_they_take() {
        // 6 KB at 4 KB/s takes 1.5 s, five times the read timeout. A total
        // timeout would cut it off; progress keeps it alive.
        let file: Vec<u8> = (0..6 * 1024).map(|i| i as u8).collect();
        let expected = file.clone();
        let server = SlowServer::start(
            Network::new(Duration::from_millis(50)).bandwidth(4 * 1024),
            move |_| Reply::bytes(file.clone()),
        );
        let timeouts = Timeouts {
            connect: Duration::from_secs(5),
            read: Duration::from_millis(300),
        };
        let client = Client::with_timeouts(&server.url(), "token", timeouts).unwrap();
        let started = Instant::now();
        let downloaded = block_on(client.download_file("file")).unwrap();
        assert!(started.elapsed() > Duration::from_secs(1));
        assert_eq!(downloaded, expected);
    }

    #[test]
    fn a_stalled_response_fails_soon_with_a_readable_error() {
        let server = SlowServer::start(Network::new(Duration::ZERO), |_| {
            Reply::json(many_channels()).stall_after(1024)
        });
        let timeouts = Timeouts {
            connect: Duration::from_secs(5),
            read: Duration::from_millis(300),
        };
        let client = Client::with_timeouts(&server.url(), "token", timeouts).unwrap();
        let started = Instant::now();
        let err = block_on(client.my_channels(&TeamId::from("team"))).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(err.to_string(), "The server took too long to respond.");
    }

    #[test]
    fn a_silent_server_fails_with_a_readable_error() {
        let server = SlowServer::start(Network::new(Duration::from_secs(30)), |_| {
            Reply::json(json!({}))
        });
        let timeouts = Timeouts {
            connect: Duration::from_secs(5),
            read: Duration::from_millis(300),
        };
        let client = Client::with_timeouts(&server.url(), "token", timeouts).unwrap();
        let err = block_on(client.me()).unwrap_err();
        assert_eq!(err.to_string(), "The server took too long to respond.");
    }

    #[test]
    fn normalizes_bare_hosts() {
        assert_eq!(
            normalize_server_url("chat.example.com").unwrap().as_str(),
            "https://chat.example.com/"
        );
        assert_eq!(
            normalize_server_url("http://localhost:8065/")
                .unwrap()
                .as_str(),
            "http://localhost:8065/"
        );
        assert_eq!(
            normalize_server_url("https://example.com/mm")
                .unwrap()
                .as_str(),
            "https://example.com/mm/"
        );
        assert!(normalize_server_url("  ").is_err());
        assert!(normalize_server_url("ftp://x").is_err());
    }
}
