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
    io::{BufRead as _, BufReader, Read as _, Write as _},
    net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use futures::channel::mpsc::UnboundedSender;
use mattermost::sso::{CALLBACK_SCHEME, MOBILE_CALLBACK_SCHEME, is_callback_url};

/// The first line the running instance sends, so a forwarding process never
/// hands a link to some other program that took over a stale port.
const GREETING: &str = "mtc-deep-link 1";
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

/// Hand `urls` to an mtc that is already running. Returns false when none
/// answers, and this process should handle them itself.
pub fn forward_to_running(urls: &[String]) -> bool {
    match try_forward(urls) {
        Ok(()) => true,
        Err(err) => {
            log::info!("no running mtc took the link ({err:#}); handling it here");
            false
        }
    }
}

fn try_forward(urls: &[String]) -> Result<()> {
    let path = instance_file().context("no config directory")?;
    let contents = fs::read_to_string(&path)?;
    let (port, secret) = contents
        .trim()
        .split_once(' ')
        .context("malformed instance file")?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port.parse::<u16>()?));
    let stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?.take(256));
    let mut greeting = String::new();
    reader.read_line(&mut greeting)?;
    if greeting.trim_end() != GREETING {
        bail!("the port belongs to another program");
    }
    let mut stream = stream;
    let mut message = format!("{secret}\n");
    for url in urls {
        message.push_str(url);
        message.push('\n');
    }
    stream.write_all(message.as_bytes())?;
    stream.flush()?;
    Ok(())
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

fn receive(mut stream: TcpStream, secret: &str) -> Result<Vec<String>> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    stream.write_all(format!("{GREETING}\n").as_bytes())?;
    let mut message = String::new();
    stream.take(MAX_MESSAGE).read_to_string(&mut message)?;
    let mut lines = message.lines();
    if lines.next() != Some(secret) {
        bail!("wrong secret");
    }
    Ok(lines
        .filter(|line| is_callback_url(line))
        .map(str::to_string)
        .collect())
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
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            receive(stream, "s3cret").unwrap()
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        let mut greeting = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut greeting)
            .unwrap();
        assert_eq!(greeting.trim_end(), GREETING);
        stream
            .write_all(b"s3cret\nmmauth://callback?MMAUTHTOKEN=t\nhttps://evil.example\n")
            .unwrap();
        drop(stream);
        assert_eq!(
            server.join().unwrap(),
            vec!["mmauth://callback?MMAUTHTOKEN=t".to_string()]
        );
    }

    #[test]
    fn rejects_a_wrong_secret() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            receive(stream, "s3cret")
        });
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .write_all(b"guess\nmmauth://callback?MMAUTHTOKEN=t\n")
            .unwrap();
        drop(stream);
        assert!(server.join().unwrap().is_err());
    }
}
