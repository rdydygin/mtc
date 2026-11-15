//! Shared live-server fixture. All application traffic passes through a recording
//! proxy: failures include actual requests, status codes, bodies and WS frames,
//! even when mtc silently discards a payload it cannot deserialize.
#![allow(dead_code)] // Each integration target uses a different part of the fixture.

use anyhow::{Context as _, Result, bail};
use futures::{SinkExt as _, StreamExt as _};
use mattermost::{Channel, Client, Event, EventStream, Post, Team, User, runtime};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::watch,
};
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest as _};

pub const PASSWORD: &str = "Live-test-Password9!";
pub const ADMIN: &str = "mtc_live_admin";
pub const PNG: &[u8] = include_bytes!("pixel.png");

pub struct Wire {
    url: String,
    task: tokio::task::JoinHandle<()>,
    sockets: watch::Sender<bool>,
    transcript: Arc<Mutex<Vec<String>>>,
}

impl Wire {
    pub async fn start(upstream: String) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let transcript = Arc::new(Mutex::new(Vec::new()));
        let log = transcript.clone();
        let (sockets, _) = watch::channel(false);
        let control = sockets.clone();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let task = tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let log = log.clone();
                let control = control.subscribe();
                let http = http.clone();
                tokio::spawn(async move {
                    if let Err(err) = forward(socket, &upstream, http, &log, control).await {
                        log.lock().unwrap().push(format!("proxy: {err:#}"));
                    }
                });
            }
        });
        Ok(Self {
            url,
            task,
            sockets,
            transcript,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    /// Hold all WebSockets down, while REST remains connected to the real server.
    pub fn disconnect(&self) {
        self.sockets.send_replace(true);
    }
    pub fn reconnect(&self) {
        self.sockets.send_replace(false);
    }
    pub fn dump(&self) -> String {
        self.transcript.lock().unwrap().join("\n")
    }
    pub fn requests(&self, method: &str, path: &str) -> usize {
        let prefix = format!("{method} {path} request:");
        self.transcript
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.starts_with(&prefix))
            .count()
    }
    pub fn frames(&self) -> Vec<Value> {
        self.transcript
            .lock()
            .unwrap()
            .iter()
            .filter_map(|line| {
                line.strip_prefix("WS <- ")
                    .and_then(|s| serde_json::from_str(s).ok())
            })
            .collect()
    }
}

impl Drop for Wire {
    fn drop(&mut self) {
        self.task.abort();
        self.disconnect();
        eprintln!("LIVE WIRE TRANSCRIPT\n{}", self.dump());
    }
}

async fn forward(
    mut socket: TcpStream,
    upstream: &str,
    http: reqwest::Client,
    log: &Arc<Mutex<Vec<String>>>,
    mut control: watch::Receiver<bool>,
) -> Result<()> {
    // One request per connection; the proxy returns Connection: close.
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(socket.read_u8().await?);
        if head.len() > 64 * 1024 {
            bail!("oversize HTTP header");
        }
    }
    let text = String::from_utf8(head.clone())?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().context("method")?;
    let path = first.next().context("path")?;
    let headers: Vec<_> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.to_lowercase(), v.trim().to_string()))
        .collect();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    if header("upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket")) {
        log.lock()
            .unwrap()
            .push(format!("GET {path} -> WebSocket upgrade"));
        if *control.borrow() {
            return Ok(());
        }
        let mut request =
            format!("{}{path}", upstream.replacen("http", "ws", 1)).into_client_request()?;
        if let Some(auth) = header("authorization") {
            request.headers_mut().insert("Authorization", auth.parse()?);
        }
        let (mut remote, response) = tokio_tungstenite::connect_async(request).await?;
        log.lock()
            .unwrap()
            .push(format!("GET {path} -> {} {response:?}", response.status()));
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(
            header("sec-websocket-key")
                .context("websocket key")?
                .as_bytes(),
        );
        socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await?;
        let mut local = tokio_tungstenite::WebSocketStream::from_raw_socket(
            socket,
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await;
        loop {
            tokio::select! {
                _ = control.changed() => { if *control.borrow() { break; } }
                msg = remote.next() => {
                    let Some(msg) = msg else { break }; let msg = msg?;
                    if let Message::Text(s) = &msg { log.lock().unwrap().push(format!("WS <- {s}")); }
                    local.send(msg).await?;
                }
                msg = local.next() => {
                    let Some(msg) = msg else { break }; let msg = msg?;
                    if let Message::Text(s) = &msg
                        && !s.contains("authentication_challenge") {
                        log.lock().unwrap().push(format!("WS -> {s}"));
                    }
                    remote.send(msg).await?;
                }
            }
        }
        return Ok(());
    }
    let len: usize = header("content-length").unwrap_or("0").parse()?;
    let mut body = vec![0; len];
    socket.read_exact(&mut body).await?;
    let logged_body = if path.ends_with("/login") || path == "/api/v4/users" {
        "<credentials redacted>".to_string()
    } else {
        String::from_utf8_lossy(&body).to_string()
    };
    let mut request = http.request(
        Method::from_bytes(method.as_bytes())?,
        format!("{upstream}{path}"),
    );
    for (k, v) in &headers {
        if !matches!(
            k.as_str(),
            "host" | "connection" | "content-length" | "accept-encoding"
        ) {
            request = request.header(k, v);
        }
    }
    log.lock().unwrap().push(format!(
        "{method} {path} request: {logged_body} (awaiting response)"
    ));
    let response =
        request.body(body).send().await.with_context(|| {
            format!("{method} {path}: no HTTP response (request: {logged_body})")
        })?;
    let status = response.status();
    let response_headers = response.headers().clone();
    let bytes = response.bytes().await?;
    log.lock().unwrap().push(format!(
        "{method} {path}\nrequest: {logged_body}\nstatus: {status}\nresponse: {}",
        String::from_utf8_lossy(&bytes)
    ));
    let mut output = format!(
        "HTTP/1.1 {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        status.as_str(),
        bytes.len()
    );
    for (k, v) in &response_headers {
        if !matches!(
            k.as_str(),
            "connection" | "content-length" | "transfer-encoding" | "content-encoding"
        ) {
            output.push_str(&format!("{k}: {}\r\n", v.to_str()?));
        }
    }
    output.push_str("\r\n");
    socket.write_all(output.as_bytes()).await?;
    socket.write_all(&bytes).await?;
    Ok(())
}

pub struct Live {
    pub wire: Wire,
    pub admin: Client,
    pub a: Client,
    pub b: Client,
    pub c: Client,
    pub alice: User,
    pub bob: User,
    pub carol: User,
    pub team: Team,
    pub public: Channel,
    pub private: Channel,
    pub suffix: String,
}

impl Live {
    pub async fn provision() -> Result<Option<Self>> {
        let Ok(server) = std::env::var("MTC_LIVE_SERVER") else {
            eprintln!("SKIP live test: set MTC_LIVE_SERVER (normally http://localhost:8065)");
            return Ok(None);
        };
        let wire = Wire::start(server.trim_end_matches('/').to_string()).await?;
        let suffix = format!(
            "{:x}{:x}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id()
        );
        // Fixed bootstrap account is the first user on an empty instance and
        // therefore admin. On reruns, authenticate it before creating anything.
        let bootstrap = reqwest::Client::new();
        let login = bootstrap
            .post(format!("{}/api/v4/users/login", wire.url))
            .json(&json!({"login_id":ADMIN,"password":PASSWORD}))
            .send()
            .await?;
        let status = login.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::BAD_REQUEST
        {
            let response = bootstrap.post(format!("{}/api/v4/users",wire.url))
                .json(&json!({"username":ADMIN,"email":"mtc_live_admin@example.test","password":PASSWORD})).send().await?;
            let status = response.status();
            let body = response.text().await?;
            if !status.is_success() {
                bail!(
                    "bootstrap POST /users: {status}: {body}; use a fresh instance or the documented bootstrap credentials"
                );
            }
        } else if !status.is_success() {
            bail!(
                "bootstrap POST /users/login: {status}: {}",
                login.text().await?
            );
        }
        let (admin, _) = Client::login(&wire.url, ADMIN, PASSWORD, None).await?;
        let config: Value = raw(&admin,"PUT","config/patch",json!({
            "TeamSettings":{"EnableOpenServer":true,"EnableUserCreation":true,"EnableCustomUserStatuses":true},
            "ServiceSettings":{"CollapsedThreads":"always_on","EnableCustomEmoji":true,"EnableCommands":true},
            "EmailSettings":{"RequireEmailVerification":false}
        })).await?;
        assert_eq!(config["ServiceSettings"]["CollapsedThreads"], "always_on");
        assert_eq!(config["TeamSettings"]["EnableCustomUserStatuses"], true);
        let mut accounts = Vec::new();
        for name in ["a", "b", "c"] {
            let username = format!("mtclive{name}{suffix}");
            let _: User = raw(&admin,"POST","users",json!({"username":username,"email":format!("{username}@example.test"),"password":PASSWORD})).await?;
            accounts.push(Client::login(&wire.url, &username, PASSWORD, None).await?);
        }
        let (a, alice) = accounts.remove(0);
        let (b, bob) = accounts.remove(0);
        let (c, carol) = accounts.remove(0);
        let team: Team = raw(
            &a,
            "POST",
            "teams",
            json!({"name":format!("live{suffix}"),"display_name":"mtc live tests","type":"O"}),
        )
        .await?;
        for user in [&bob, &carol] {
            let _: Value = raw(
                &a,
                "POST",
                &format!("teams/{}/members", team.id),
                json!({"team_id":team.id,"user_id":user.id}),
            )
            .await?;
        }
        let public = Self::create_channel(&a, &team, "public", &suffix, "O").await?;
        let private = Self::create_channel(&a, &team, "private", &suffix, "P").await?;
        // Carol stays outside the public channel for autocomplete's out_of_channel.
        a.join_channel(&public.id, &bob.id).await?;
        a.join_channel(&private.id, &bob.id).await?;
        Ok(Some(Self {
            wire,
            admin,
            a,
            b,
            c,
            alice,
            bob,
            carol,
            team,
            public,
            private,
            suffix,
        }))
    }

    pub async fn create_channel(
        client: &Client,
        team: &Team,
        name: &str,
        suffix: &str,
        kind: &str,
    ) -> Result<Channel> {
        raw(client,"POST","channels",json!({"team_id":team.id,"name":format!("{name}{suffix}"),"display_name":name,"type":kind})).await
    }
    pub async fn post(&self, client: &Client, text: &str) -> Result<Post> {
        client
            .create_post(&self.public.id, text, None, "", &[], &Default::default())
            .await
    }
}

/// Setup and contracts not exposed by mtc. Also logged by Wire.
pub async fn raw<T: DeserializeOwned>(
    client: &Client,
    method: &str,
    path: &str,
    body: Value,
) -> Result<T> {
    let response = reqwest::Client::new()
        .request(
            Method::from_bytes(method.as_bytes())?,
            client.server().join(&format!("api/v4/{path}"))?,
        )
        .bearer_auth(client.token())
        .json(&body)
        .send()
        .await?;
    let status = response.status();
    let text = response.text().await?;
    if !status.is_success() {
        bail!("{method} /api/v4/{path} request={body} status={status} response={text}");
    }
    serde_json::from_str(&text)
        .with_context(|| format!("{method} {path} request={body} status={status} response={text}"))
}

pub type Events = futures::channel::mpsc::UnboundedReceiver<Event>;
pub async fn connected(client: &Client) -> (EventStream, Events) {
    let (stream, mut events) = EventStream::connect(client.clone());
    event(&mut events, "hello", |e| matches!(e, Event::Connected)).await;
    (stream, events)
}
pub async fn event(events: &mut Events, label: &str, predicate: impl Fn(&Event) -> bool) -> Event {
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = events.next().await {
            if predicate(&event) {
                return event;
            }
        }
        panic!("WS closed waiting for {label}");
    })
    .await
    .unwrap_or_else(|_| panic!("WS timeout waiting for {label}; see LIVE WIRE TRANSCRIPT"))
}

/// Run fixture setup/raw API from a GPUI test without borrowing its executor.
pub fn io<T: Send + 'static>(
    future: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> T {
    futures::executor::block_on(runtime::run(future)).unwrap()
}
