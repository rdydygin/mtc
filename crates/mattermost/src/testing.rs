//! A stand-in Mattermost server behind a slow network, for tests.
//!
//! [`SlowServer`] answers HTTP/1.1 on a local port. Every response waits
//! out the network's latency first, then trickles its body at the network's
//! bandwidth, so tests can check how the client and the session behave on a
//! poor connection: which requests run in parallel, how long startup takes
//! in round trips, whether slow downloads survive and stalled ones fail.
//!
//! Enabled by the `test-support` feature.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use crate::runtime;

/// The conditions between client and server.
#[derive(Clone, Copy, Debug)]
pub struct Network {
    latency: Duration,
    bytes_per_second: Option<u64>,
}

impl Network {
    /// A link that delays every response by `latency` (one round trip).
    pub fn new(latency: Duration) -> Self {
        Self {
            latency,
            bytes_per_second: None,
        }
    }

    /// Limit how fast response bodies arrive.
    pub fn bandwidth(mut self, bytes_per_second: u64) -> Self {
        self.bytes_per_second = Some(bytes_per_second.max(1));
        self
    }
}

/// A request as the server saw it.
#[derive(Clone, Debug)]
pub struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// When it arrived, from the server's start.
    at: Duration,
}

impl Request {
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The path and query, such as `/api/v4/users/me`.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// A header's value, by case-insensitive name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The request body after it has been read in full.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// When the request arrived, measured from the server's start.
    pub fn at(&self) -> Duration {
        self.at
    }
}

/// What the server sends back.
#[derive(Clone, Debug)]
pub struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    stall_after: Option<usize>,
}

impl Reply {
    pub fn json(value: serde_json::Value) -> Self {
        Self::bytes(value.to_string().into_bytes()).header("Content-Type", "application/json")
    }

    pub fn bytes(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body,
            stall_after: None,
        }
    }

    pub fn not_found() -> Self {
        Self::json(serde_json::json!({ "message": "Not found" })).status(404)
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Send the headers and the first `bytes` of the body, then go silent
    /// without closing the connection, like a link that dropped.
    pub fn stall_after(mut self, bytes: usize) -> Self {
        self.stall_after = Some(bytes);
        self
    }
}

type Route = dyn Fn(&Request) -> Reply + Send + Sync;

/// A local HTTP server with simulated latency and bandwidth.
pub struct SlowServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl Drop for SlowServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl SlowServer {
    /// Start serving on a free local port. `route` answers each request.
    pub fn start(
        network: Network,
        route: impl Fn(&Request) -> Reply + Send + Sync + 'static,
    ) -> Self {
        let std_listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("bind a local port for the server");
        std_listener
            .set_nonblocking(true)
            .expect("make the listener non-blocking");
        let addr = std_listener.local_addr().expect("read the server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let route: Arc<Route> = Arc::new(route);
        let log = requests.clone();
        let started = Instant::now();
        let task = runtime::spawn(async move {
            let listener = TcpListener::from_std(std_listener).expect("adopt the listener");
            while let Ok((stream, _)) = listener.accept().await {
                let route = route.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let _ = serve(stream, network, started, &*route, &log).await;
                });
            }
        });
        Self {
            addr,
            requests,
            task,
        }
    }

    /// The server's base URL, as a person would type it.
    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Every request so far, in arrival order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

/// Answer requests on one connection until the client closes it.
async fn serve(
    mut stream: TcpStream,
    network: Network,
    started: Instant,
    route: &Route,
    log: &Mutex<Vec<Request>>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut buffer = Vec::new();
    loop {
        let Some((mut request, body_len, consumed)) =
            read_head(&mut stream, &mut buffer, started).await?
        else {
            return Ok(());
        };
        // Read the body in full before routing or recording the request.
        while buffer.len() < consumed + body_len {
            let mut chunk = [0u8; 8192];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buffer.extend_from_slice(&chunk[..n]);
        }
        request.body = buffer[consumed..consumed + body_len].to_vec();
        buffer.drain(..consumed + body_len);
        log.lock().unwrap().push(request.clone());

        let reply = route(&request);
        tokio::time::sleep(network.latency).await;
        let mut head = format!(
            "HTTP/1.1 {} {}\r\nContent-Length: {}\r\n",
            reply.status,
            if reply.status < 400 { "OK" } else { "Error" },
            reply.body.len()
        );
        for (name, value) in &reply.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await?;

        let body = match reply.stall_after {
            Some(n) => &reply.body[..n.min(reply.body.len())],
            None => &reply.body[..],
        };
        match network.bytes_per_second {
            None => stream.write_all(body).await?,
            Some(rate) => {
                // Twenty slices a second keeps the trickle smooth.
                let slice = ((rate / 20) as usize).max(1);
                for chunk in body.chunks(slice) {
                    stream.write_all(chunk).await?;
                    stream.flush().await?;
                    tokio::time::sleep(Duration::from_secs_f64(chunk.len() as f64 / rate as f64))
                        .await;
                }
            }
        }
        stream.flush().await?;
        if reply.stall_after.is_some() {
            // Hold the connection open and say nothing more.
            std::future::pending::<()>().await;
        }
    }
}

/// Read one request head. Returns the request, its body length, and how
/// many buffered bytes the head took, or `None` when the client closed.
async fn read_head(
    stream: &mut TcpStream,
    buffer: &mut Vec<u8>,
    started: Instant,
) -> std::io::Result<Option<(Request, usize, usize)>> {
    loop {
        if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_string();
            let mut lines = head.split("\r\n");
            let mut start = lines.next().unwrap_or_default().split_whitespace();
            let method = start.next().unwrap_or_default().to_string();
            let path = start.next().unwrap_or_default().to_string();
            let headers: Vec<(String, String)> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(n, v)| (n.trim().to_string(), v.trim().to_string()))
                .collect();
            let body_len = headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(0);
            let request = Request {
                method,
                path,
                headers,
                body: Vec::new(),
                at: started.elapsed(),
            };
            return Ok(Some((request, body_len, end + 4)));
        }
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buffer.extend_from_slice(&chunk[..n]);
    }
}
