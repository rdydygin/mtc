//! SSO callback links on Linux and Windows.
//!
//! On macOS the app bundle declares the `mattermost-dev://` and `mmauth://`
//! schemes, and GPUI hands the links to the running app (`on_open_urls`).
//! Linux and Windows work differently: GPUI registers no schemes there, and
//! the system opens a link by starting the registered program with the link
//! as an argument. This module fills both gaps:
//!
//! - [`register`] points the schemes at this executable: a desktop entry and
//!   `xdg-mime` on Linux, per-user `Software\Classes` keys on Windows.
//! - [`listen`] lets the running mtc receive links, and
//!   [`forward_to_running`] lets the process the browser starts pass its link
//!   to that instance and exit. They talk over a loopback socket whose port
//!   and secret live in an owner-only file in the config directory.
//!
//! A link is only acted on when the sign-in panel is waiting for one, as on
//! macOS (see `sso::handle_open_urls`).

use std::{
    fs,
    io::{self, BufRead as _, BufReader, Read as _, Write as _},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use futures::channel::mpsc::UnboundedSender;
use hmac::{Hmac, Mac as _};
use mattermost::sso::{CALLBACK_SCHEME, MOBILE_CALLBACK_SCHEME, is_callback_url};
use sha2::Sha256;

/// Version 2 proves possession of the instance secret before sending callback links.
const GREETING: &str = "mtc-deep-link 2";
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Callback links are short; anything longer is not one of ours.
const MAX_MESSAGE: u64 = 64 * 1024;

/// SSO callback links among the process arguments, as the system passes
/// them when it opens a link with mtc.
pub fn urls_from_args() -> Vec<String> {
    std::env::args()
        .skip(1)
        .filter(|arg| is_callback_url(arg))
        .collect()
}

fn instance_file() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("mtc").join("instance"))
}

/// Whether a callback process may start its own UI and listener.
#[derive(Debug, PartialEq, Eq)]
pub enum ForwardOutcome {
    Absent,
    Forwarded,
    Incompatible,
    AuthenticationFailed,
}

impl ForwardOutcome {
    pub fn should_exit(&self) -> bool {
        *self != Self::Absent
    }
}

/// Hand callbacks to the running instance, preserving its rendezvous file on failure.
pub fn forward_to_running(urls: &[String]) -> ForwardOutcome {
    let Some(path) = instance_file() else {
        return ForwardOutcome::Absent;
    };
    forward_from(&path, urls)
}

fn forward_from(path: &Path, urls: &[String]) -> ForwardOutcome {
    match try_forward(path, urls) {
        Ok(ForwardOutcome::Incompatible) => {
            log::error!(
                "The running mtc uses a different sign-in protocol. Quit it, reopen mtc, \
                 and retry sign-in, or paste the sign-in link into its waiting sign-in panel."
            );
            ForwardOutcome::Incompatible
        }
        Ok(outcome) => outcome,
        Err(err) => {
            log::error!(
                "Couldn’t verify the running mtc’s sign-in handoff ({err:#}). Quit all mtc instances, \
                 reopen mtc, and retry sign-in."
            );
            ForwardOutcome::AuthenticationFailed
        }
    }
}

fn try_forward(path: &Path, urls: &[String]) -> Result<ForwardOutcome> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(ForwardOutcome::Absent),
        Err(err) => return Err(err.into()),
    };
    let (port, secret) = contents
        .trim()
        .split_once(' ')
        .context("malformed instance file")?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port.parse::<u16>()?));
    let stream = match TcpStream::connect_timeout(&addr, IO_TIMEOUT) {
        Ok(stream) => stream,
        Err(err) if err.kind() == io::ErrorKind::ConnectionRefused => {
            return Ok(ForwardOutcome::Absent);
        }
        Err(err) => return Err(err.into()),
    };
    forward(stream, secret, urls)
}

fn forward(stream: TcpStream, secret: &str, urls: &[String]) -> Result<ForwardOutcome> {
    forward_with_timeout(stream, secret, urls, IO_TIMEOUT)
}

fn forward_with_timeout(
    mut stream: TcpStream,
    secret: &str,
    urls: &[String],
    timeout: Duration,
) -> Result<ForwardOutcome> {
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let nonce = new_secret()?;
    stream.write_all(format!("{GREETING}\n{nonce}\n").as_bytes())?;
    let mut reader = BufReader::new(stream);
    let greeting = read_line(&mut reader)?;
    if greeting != GREETING {
        if greeting.starts_with("mtc-deep-link ") {
            return Ok(ForwardOutcome::Incompatible);
        }
        bail!("the port belongs to another program");
    }
    let challenge = read_line(&mut reader)?;
    let listener_proof = read_line(&mut reader)?;
    verify(secret, "listener", &nonce, &challenge, "", &listener_proof)?;

    let message: String = urls.iter().map(|url| format!("{url}\n")).collect();
    if message.len() as u64 > MAX_MESSAGE {
        bail!("too many callback links");
    }
    let proof = proof(secret, "callback", &nonce, &challenge, &message);
    let stream = reader.get_mut();
    stream.write_all(format!("{proof}\n{message}").as_bytes())?;
    stream.shutdown(Shutdown::Write)?;
    if read_line(&mut reader)? != "ok" {
        bail!("the listener didn't accept the links");
    }
    Ok(ForwardOutcome::Forwarded)
}

/// Accept links from processes the system starts for them, and send them
/// to `urls`. Runs on its own thread for the life of the app.
pub fn listen(urls: UnboundedSender<Vec<String>>) {
    let result = (|| -> Result<()> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let secret = new_secret()?;
        write_instance_file(&format!("{port} {secret}\n"))?;
        std::thread::Builder::new()
            .name("mtc-deep-link".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    match receive(stream, &secret) {
                        Ok(received) if !received.is_empty() => {
                            if urls.unbounded_send(received).is_err() {
                                return;
                            }
                        }
                        Ok(_) => {}
                        Err(err) => log::warn!("ignoring a deep-link connection: {err:#}"),
                    }
                }
            })?;
        Ok(())
    })();
    if let Err(err) = result {
        log::warn!("can't receive SSO links from the browser ({err:#}); paste them instead");
    }
}

fn receive(stream: TcpStream, secret: &str) -> Result<Vec<String>> {
    receive_until(stream, secret, Instant::now() + IO_TIMEOUT)
}

fn receive_until(stream: TcpStream, secret: &str, deadline: Instant) -> Result<Vec<String>> {
    receive_from(
        &mut BufReader::new(DeadlineStream { stream, deadline }),
        secret,
    )
}

fn receive_from(
    reader: &mut BufReader<impl io::Read + io::Write>,
    secret: &str,
) -> Result<Vec<String>> {
    if read_line(reader)? != GREETING {
        bail!("unsupported deep-link protocol");
    }
    let nonce = read_line(reader)?;
    let challenge = new_secret()?;
    let proof = proof(secret, "listener", &nonce, &challenge, "");
    reader
        .get_mut()
        .write_all(format!("{GREETING}\n{challenge}\n{proof}\n").as_bytes())?;
    let proof = read_line(reader)?;
    let mut message = String::new();
    reader
        .by_ref()
        .take(MAX_MESSAGE + 1)
        .read_to_string(&mut message)?;
    if message.len() as u64 > MAX_MESSAGE {
        bail!("too many callback links");
    }
    verify(secret, "callback", &nonce, &challenge, &message, &proof)?;
    reader.get_mut().write_all(b"ok\n")?;
    Ok(message
        .lines()
        .filter(|line| is_callback_url(line))
        .map(str::to_string)
        .collect())
}

/// Keep every read and write within the connection's original deadline.
struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}

impl DeadlineStream {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "deep-link connection deadline elapsed",
                )
            })
    }
}

impl io::Read for DeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}

impl io::Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

/// Bound every handshake line, including one from an unrelated local process.
fn read_line(reader: &mut impl io::BufRead) -> Result<String> {
    let mut line = String::new();
    reader.take(256).read_line(&mut line)?;
    if !line.ends_with('\n') {
        bail!("incomplete deep-link handshake");
    }
    line.pop();
    Ok(line)
}

fn mac(secret: &str, role: &str, nonce: &str, challenge: &str, message: &str) -> Hmac<Sha256> {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key size");
    for part in [GREETING, role, nonce, challenge, message] {
        mac.update(&(part.len() as u64).to_be_bytes());
        mac.update(part.as_bytes());
    }
    mac
}

fn proof(secret: &str, role: &str, nonce: &str, challenge: &str, message: &str) -> String {
    mac(secret, role, nonce, challenge, message)
        .finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn verify(
    secret: &str,
    role: &str,
    nonce: &str,
    challenge: &str,
    message: &str,
    proof: &str,
) -> Result<()> {
    if proof.len() != 64 {
        bail!("invalid deep-link proof");
    }
    let mut bytes = [0; 32];
    for (ix, byte) in bytes.iter_mut().enumerate() {
        let pair = proof
            .as_bytes()
            .get(ix * 2..ix * 2 + 2)
            .context("invalid deep-link proof")?;
        let pair = std::str::from_utf8(pair)?;
        *byte = u8::from_str_radix(pair, 16)?;
    }
    mac(secret, role, nonce, challenge, message)
        .verify_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("wrong deep-link proof"))
}

fn new_secret() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|err| anyhow::anyhow!("no randomness: {err}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn write_instance_file(contents: &str) -> Result<()> {
    let path = instance_file().context("no config directory")?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(&path)?.write_all(contents.as_bytes())?;
    Ok(())
}

/// Make the system open SSO callback links with this executable. Runs in
/// the background; failure only means the person pastes the link instead.
pub fn register() {
    std::thread::spawn(|| {
        if let Err(err) = register_schemes() {
            log::info!(
                "SSO callback schemes not registered ({err:#}); paste-the-link fallback only"
            );
        }
    });
}

#[cfg(target_os = "windows")]
fn register_schemes() -> Result<()> {
    let exe = std::env::current_exe()?;
    let command = format!("\"{}\" \"%1\"", exe.display());
    for scheme in [CALLBACK_SCHEME, MOBILE_CALLBACK_SCHEME] {
        let classes = format!("Software\\Classes\\{scheme}");
        let key = windows_registry::CURRENT_USER.create(&classes)?;
        key.set_string("", "URL:mtc sign-in callback")?;
        key.set_string("URL Protocol", "")?;
        windows_registry::CURRENT_USER
            .create(format!("{classes}\\shell\\open\\command"))?
            .set_string("", &command)?;
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
const DESKTOP_FILE: &str = "mtc-sso-callback.desktop";

#[cfg(not(target_os = "windows"))]
fn register_schemes() -> Result<()> {
    use std::process::Command;

    let exe = std::env::current_exe()?;
    let exe = exe.to_str().context("the executable path isn't UTF-8")?;
    let dir = dirs::data_dir()
        .context("no data directory")?
        .join("applications");
    let path = dir.join(DESKTOP_FILE);
    let entry = desktop_entry(exe);
    let changed = fs::read_to_string(&path).ok().as_deref() != Some(entry.as_str());
    if changed {
        fs::create_dir_all(&dir)?;
        fs::write(&path, &entry)?;
        // Refreshes the MIME cache some desktops read; harmless if missing.
        let _ = Command::new("update-desktop-database").arg(&dir).status();
    }
    for scheme in [CALLBACK_SCHEME, MOBILE_CALLBACK_SCHEME] {
        let mime = format!("x-scheme-handler/{scheme}");
        let current = Command::new("xdg-mime")
            .args(["query", "default", &mime])
            .output()
            .context("xdg-mime isn't installed")?;
        if String::from_utf8_lossy(&current.stdout).trim() == DESKTOP_FILE {
            continue;
        }
        let status = Command::new("xdg-mime")
            .args(["default", DESKTOP_FILE, &mime])
            .status()?;
        if !status.success() {
            bail!("xdg-mime couldn't set the handler for {scheme}://");
        }
    }
    Ok(())
}

/// A hidden desktop entry that opens our callback schemes with `exe`.
#[cfg(not(target_os = "windows"))]
fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=mtc\n\
         Comment=Completes single sign-on in mtc\n\
         Exec={} %u\n\
         Terminal=false\n\
         NoDisplay=true\n\
         MimeType=x-scheme-handler/{CALLBACK_SCHEME};x-scheme-handler/{MOBILE_CALLBACK_SCHEME};\n",
        desktop_exec_arg(exe)
    )
}

/// Quote an argument for a desktop entry's `Exec` key. The quoting rule
/// escapes `"`, `` ` ``, `$`, and `\` with a backslash; the string-value rule
/// then doubles every backslash; `%` is a field code unless doubled.
#[cfg(not(target_os = "windows"))]
fn desktop_exec_arg(arg: &str) -> String {
    let mut out = String::from("\"");
    for c in arg.chars() {
        match c {
            '"' | '`' | '$' => {
                out.push_str("\\\\");
                out.push(c);
            }
            '\\' => out.push_str("\\\\\\\\"),
            '%' => out.push_str("%%"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn quotes_exec_paths() {
        assert_eq!(desktop_exec_arg("/usr/bin/mtc"), "\"/usr/bin/mtc\"");
        assert_eq!(
            desktop_exec_arg("/home/a b/100%/$x\\y"),
            "\"/home/a b/100%%/\\\\$x\\\\\\\\y\""
        );
        let entry = desktop_entry("/opt/mtc");
        assert!(entry.contains("Exec=\"/opt/mtc\" %u\n"));
        assert!(entry.contains("x-scheme-handler/mattermost-dev;x-scheme-handler/mmauth;"));
    }

    #[test]
    fn hands_links_to_the_listener() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server =
            std::thread::spawn(move || receive(listener.accept().unwrap().0, "s3cret").unwrap());
        forward(
            TcpStream::connect(addr).unwrap(),
            "s3cret",
            &[
                "mmauth://callback?MMAUTHTOKEN=t".into(),
                "mattermost-dev://callback?client_token=c&server_token=t".into(),
                "https://evil.example".into(),
            ],
        )
        .unwrap();
        assert_eq!(
            server.join().unwrap(),
            vec![
                "mmauth://callback?MMAUTHTOKEN=t".to_string(),
                "mattermost-dev://callback?client_token=c&server_token=t".to_string(),
            ]
        );
    }

    #[test]
    fn rejects_a_wrong_secret() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || receive(listener.accept().unwrap().0, "s3cret"));
        assert!(
            forward(
                TcpStream::connect(addr).unwrap(),
                "guess",
                &["mmauth://callback?MMAUTHTOKEN=t".into()]
            )
            .is_err()
        );
        assert!(server.join().unwrap().is_err());
    }

    #[test]
    fn callback_startup_preserves_incompatible_and_untrusted_instances() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!("deep-link-{}", new_secret().unwrap()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("instance");
        assert_eq!(forward_from(&path, &[]), ForwardOutcome::Absent);
        assert!(!ForwardOutcome::Absent.should_exit());
        for (reply, expected) in [
            ("mtc-deep-link 1\n", ForwardOutcome::Incompatible),
            (
                "mtc-deep-link 2\nchallenge\ninvalid-proof\n",
                ForwardOutcome::AuthenticationFailed,
            ),
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let contents = format!("{} secret\n", listener.local_addr().unwrap().port());
            fs::write(&path, &contents).unwrap();
            let server = std::thread::spawn(move || {
                let mut reader = BufReader::new(listener.accept().unwrap().0);
                read_line(&mut reader).unwrap();
                read_line(&mut reader).unwrap();
                reader.get_mut().write_all(reply.as_bytes()).unwrap();
                let mut rest = String::new();
                reader.read_to_string(&mut rest).unwrap();
                rest
            });
            let outcome = forward_from(&path, &["mmauth://callback?MMAUTHTOKEN=private".into()]);
            assert_eq!(outcome, expected);
            assert!(
                outcome.should_exit(),
                "must exit before creating another listener"
            );
            assert_eq!(fs::read_to_string(&path).unwrap(), contents);
            assert!(server.join().unwrap().is_empty());
        }
        let stale = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        fs::write(
            &path,
            format!("{} secret\n", stale.local_addr().unwrap().port()),
        )
        .unwrap();
        drop(stale);
        assert_eq!(forward_from(&path, &[]), ForwardOutcome::Absent);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Deliver one byte per read and expire the deadline at a chosen protocol point.
    struct ExpiringPeer {
        stream: DeadlineStream,
        read_count: usize,
        expire_after: usize,
    }

    impl io::Read for ExpiringPeer {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            let len = bytes.len().min(1);
            let count = self.stream.read(&mut bytes[..len])?;
            self.read_count += count;
            if self.read_count == self.expire_after {
                self.stream.deadline = Instant::now();
            }
            Ok(count)
        }
    }

    impl io::Write for ExpiringPeer {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.stream.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }

    /// Match the fixture's client even if another test retries a reused loopback port.
    fn accept_peer(listener: &TcpListener, peer: SocketAddr) -> TcpStream {
        loop {
            let (stream, addr) = listener.accept().unwrap();
            if addr == peer {
                return stream;
            }
        }
    }

    #[test]
    fn slow_peers_cannot_hold_the_listener_during_handshake_or_body() {
        // This is a deadlock watchdog, not the deadline being tested.
        const WATCHDOG: Duration = Duration::from_secs(30);
        let handshake = format!("{GREETING}\nnonce\nproof\n");
        for body in [false, true] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let expire_after = if body { handshake.len() + 1 } else { 1 };
            let mut peer = TcpStream::connect(addr).unwrap();
            peer.set_write_timeout(Some(WATCHDOG)).unwrap();
            let peer_addr = peer.local_addr().unwrap();
            let (done, result) = std::sync::mpsc::channel();
            let (next_peer, recovery_peer) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let stream = accept_peer(&listener, peer_addr);
                let mut reader = BufReader::new(ExpiringPeer {
                    stream: DeadlineStream {
                        stream,
                        deadline: Instant::now() + WATCHDOG,
                    },
                    read_count: 0,
                    expire_after,
                });
                let err = receive_from(&mut reader, "secret").unwrap_err();
                assert_eq!(
                    reader.get_ref().read_count,
                    expire_after,
                    "must reject the next byte even while the peer has more data ready"
                );
                assert_eq!(
                    err.downcast_ref::<io::Error>().unwrap().kind(),
                    io::ErrorKind::TimedOut
                );
                drop(reader);
                done.send(()).unwrap();
                receive_until(
                    accept_peer(&listener, recovery_peer.recv_timeout(WATCHDOG).unwrap()),
                    "secret",
                    Instant::now() + WATCHDOG,
                )
                .unwrap()
            });
            // Keep the peer open with bytes queued beyond expiry, so neither EOF
            // nor an idle-read timeout can stand in for the absolute deadline.
            peer.write_all(format!("{handshake}xx").as_bytes()).unwrap();
            result.recv_timeout(WATCHDOG).unwrap();
            drop(peer);
            // A retry from another test may reach a reused ephemeral port first.
            let mut unrelated = TcpStream::connect(addr).unwrap();
            unrelated.set_write_timeout(Some(WATCHDOG)).unwrap();
            unrelated.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
            let recovery = TcpStream::connect(addr).unwrap();
            next_peer.send(recovery.local_addr().unwrap()).unwrap();
            assert_eq!(
                forward_with_timeout(
                    recovery,
                    "secret",
                    &["mmauth://callback?MMAUTHTOKEN=t".into()],
                    WATCHDOG,
                )
                .unwrap(),
                ForwardOutcome::Forwarded
            );
            assert_eq!(
                server.join().unwrap(),
                vec!["mmauth://callback?MMAUTHTOKEN=t"]
            );
        }
    }

    #[test]
    fn listener_rejects_tampered_callbacks_and_reflected_proofs() {
        for reflect in [false, true] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let server =
                std::thread::spawn(move || receive(listener.accept().unwrap().0, "secret"));
            let mut reader = BufReader::new(TcpStream::connect(addr).unwrap());
            reader
                .get_mut()
                .write_all(format!("{GREETING}\nnonce\n").as_bytes())
                .unwrap();
            assert_eq!(read_line(&mut reader).unwrap(), GREETING);
            let challenge = read_line(&mut reader).unwrap();
            let listener_proof = read_line(&mut reader).unwrap();
            let proof = if reflect {
                listener_proof
            } else {
                proof(
                    "secret",
                    "callback",
                    "nonce",
                    &challenge,
                    "mmauth://callback?MMAUTHTOKEN=original\n",
                )
            };
            reader
                .get_mut()
                .write_all(format!("{proof}\nmmauth://callback?MMAUTHTOKEN=changed\n").as_bytes())
                .unwrap();
            reader.get_mut().shutdown(Shutdown::Write).unwrap();
            assert!(server.join().unwrap().is_err());
        }
    }

    #[test]
    fn impostor_listener_never_receives_the_secret_or_callback() {
        // Both the old fixed greeting and a forged version 2 proof must fail closed.
        for reply in [
            "mtc-deep-link 1\n",
            "mtc-deep-link 2\nchallenge\n0000000000000000000000000000000000000000000000000000000000000000\n",
        ] {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            let impostor = std::thread::spawn(move || {
                let mut stream = listener.accept().unwrap().0;
                stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let greeting = read_line(&mut reader).unwrap();
                let nonce = read_line(&mut reader).unwrap();
                stream.write_all(reply.as_bytes()).unwrap();
                let mut remaining = String::new();
                let _ = reader.read_to_string(&mut remaining);
                (greeting, nonce, remaining)
            });
            let outcome = forward(
                TcpStream::connect(addr).unwrap(),
                "s3cret",
                &["mmauth://callback?MMAUTHTOKEN=private-token".into()],
            );
            if reply.starts_with("mtc-deep-link 1") {
                assert_eq!(outcome.unwrap(), ForwardOutcome::Incompatible);
            } else {
                assert!(outcome.is_err());
            }
            let (greeting, nonce, remaining) = impostor.join().unwrap();
            assert_eq!(greeting, GREETING);
            assert_eq!(nonce.len(), 32);
            assert!(!nonce.contains("s3cret"));
            assert!(remaining.is_empty(), "sent sensitive data: {remaining}");
        }
    }

    #[test]
    fn proofs_bind_the_role_nonces_and_callback() {
        let message = "mmauth://callback?MMAUTHTOKEN=t\n";
        let signed = proof("secret", "callback", "nonce", "challenge", message);
        assert!(verify("secret", "callback", "nonce", "challenge", message, &signed).is_ok());
        for (role, nonce, challenge, message) in [
            ("listener", "nonce", "challenge", message),
            ("callback", "other", "challenge", message),
            ("callback", "nonce", "other", message),
            (
                "callback",
                "nonce",
                "challenge",
                "mmauth://callback?MMAUTHTOKEN=other\n",
            ),
        ] {
            assert!(verify("secret", role, nonce, challenge, message, &signed).is_err());
        }
        assert!(
            verify(
                "secret",
                "callback",
                "nonce",
                "challenge",
                message,
                &"é".repeat(32)
            )
            .is_err()
        );
    }
}
