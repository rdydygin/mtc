//! Fetching the release feed, downloading a release, and putting it in place
//! of the running app bundle.
//!
//! Network calls run on the Mattermost client's I/O runtime (see
//! [`mattermost::runtime`]); the file work is blocking and runs off the UI
//! thread on that runtime's blocking pool.

use std::{path::Path, time::Duration};

use anyhow::{Context as _, Result, ensure};
use semver::Version;
use sha2::{Digest as _, Sha256};

use super::manifest::{Download, Manifest};

/// No release comes close; a response this large is not one of ours.
const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("mtc/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        // Silence, not size: a large download on a slow link still finishes.
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("failed to build the HTTP client")
}

/// The newest release, or `None` when the feed has none yet.
pub async fn fetch_manifest(url: String) -> Result<Option<Manifest>> {
    mattermost::runtime::run(async move {
        let response = http()?
            .get(&url)
            .send()
            .await
            .context("couldn’t reach the release feed")?;
        // Releases published before the feed existed have no `latest.json`.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let bytes = response
            .error_for_status()
            .context("the release feed is unavailable")?
            .bytes()
            .await
            .context("couldn’t read the release feed")?;
        Manifest::parse(&bytes).map(Some)
    })
    .await
}

/// Download a release archive and check it against the feed's checksum.
pub async fn download(download: Download) -> Result<Vec<u8>> {
    mattermost::runtime::run(async move {
        let mut response = http()?
            .get(&download.url)
            .send()
            .await
            .context("couldn’t download the update")?
            .error_for_status()
            .context("couldn’t download the update")?;
        let mut bytes = Vec::new();
        let mut hasher = Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("the update download was interrupted")?
        {
            hasher.update(&chunk);
            bytes.extend_from_slice(&chunk);
            ensure!(bytes.len() <= MAX_ARCHIVE_BYTES, "the update is too large");
        }
        verify(&hasher.finalize(), &download.sha256)?;
        Ok(bytes)
    })
    .await
}

fn verify(digest: &[u8], expected: &str) -> Result<()> {
    let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "the update was damaged in transit (checksum mismatch)"
    );
    Ok(())
}

/// Unpack `archive` and put the app inside it in place of the bundle at
/// `target`. Finishes before returning; the running process keeps using the
/// files it already opened, and the next launch starts the new version.
#[cfg(target_os = "macos")]
pub async fn install(archive: Vec<u8>, version: Version, target: std::path::PathBuf) -> Result<()> {
    mattermost::runtime::run(async move {
        tokio::task::spawn_blocking(move || macos::install(&archive, &version, &target))
            .await
            .context("the update was cancelled")?
    })
    .await
}

#[cfg(not(target_os = "macos"))]
pub async fn install(_: Vec<u8>, _: Version, _: std::path::PathBuf) -> Result<()> {
    anyhow::bail!("this platform can’t install updates by itself")
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{fs, path::Path, process::Command};

    use anyhow::{Context as _, Result, ensure};
    use semver::Version;

    pub fn install(archive: &[u8], version: &Version, target: &Path) -> Result<()> {
        let parent = target.parent().context("the app has no enclosing folder")?;
        // Unpack beside the app, so the swap is a rename on one volume.
        let staging = parent.join(format!(".mtc-update-{version}"));
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir(&staging)
            .with_context(|| format!("mtc can’t write to {}", parent.display()))?;
        let result = (|| {
            let zip = staging.join("update.zip");
            fs::write(&zip, archive).context("couldn’t save the update")?;
            // ditto keeps the bundle's symlinks, permissions and signature.
            run(Command::new("/usr/bin/ditto")
                .args(["-x", "-k"])
                .arg(&zip)
                .arg(&staging))
            .context("couldn’t unpack the update")?;
            let app = staging.join("mtc.app");
            check_bundle(&app, target, version)?;
            super::swap(target, &app)
        })();
        let _ = fs::remove_dir_all(&staging);
        result
    }

    /// The unpacked app is this app, at the promised version, with its
    /// signature intact.
    fn check_bundle(app: &Path, current: &Path, version: &Version) -> Result<()> {
        let id = |bundle: &Path| plist_value(bundle, "CFBundleIdentifier");
        ensure!(
            id(app)? == id(current)?,
            "the update is for a different app"
        );
        ensure!(
            plist_value(app, "CFBundleShortVersionString")? == version.to_string(),
            "the update’s version doesn’t match the release"
        );
        run(Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(app))
        .context("the update’s signature is broken")
    }

    fn plist_value(bundle: &Path, key: &str) -> Result<String> {
        let output = Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(bundle.join("Contents/Info.plist"))
            .output()
            .context("couldn’t run plutil")?;
        ensure!(output.status.success(), "{} has no {key}", bundle.display());
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    fn run(command: &mut Command) -> Result<()> {
        let output = command.output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(())
    }
}

/// Put the bundle at `new` where the one at `target` is. The old bundle is
/// moved aside first and put back if the new one can't take its place, so a
/// failure never leaves the app missing.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn swap(target: &Path, new: &Path) -> Result<()> {
    let name = target
        .file_name()
        .context("the app has no file name")?
        .to_string_lossy();
    let aside = target.with_file_name(format!(".{name}.old"));
    let _ = std::fs::remove_dir_all(&aside);
    std::fs::rename(target, &aside)
        .with_context(|| format!("mtc can’t replace {}", target.display()))?;
    if let Err(err) = std::fs::rename(new, target) {
        let _ = std::fs::rename(&aside, target);
        return Err(err).with_context(|| format!("mtc can’t replace {}", target.display()));
    }
    if let Err(err) = std::fs::remove_dir_all(&aside) {
        log::warn!("removing the previous version: {err}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mtc-update-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bundle(at: &Path, version: &str) {
        fs::create_dir_all(at.join("Contents/MacOS")).unwrap();
        fs::write(at.join("Contents/MacOS/mtc"), version).unwrap();
    }

    fn version_at(at: &Path) -> String {
        fs::read_to_string(at.join("Contents/MacOS/mtc")).unwrap()
    }

    #[test]
    fn swap_replaces_the_bundle_and_removes_the_old_one() {
        let dir = scratch("swap");
        let target = dir.join("mtc.app");
        let new = dir.join(".staging/mtc.app");
        bundle(&target, "0.1.0");
        bundle(&new, "0.2.0");

        swap(&target, &new).unwrap();

        assert_eq!(version_at(&target), "0.2.0");
        assert!(!new.exists());
        assert!(!dir.join(".mtc.app.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_swap_keeps_the_old_bundle() {
        let dir = scratch("swap-fail");
        let target = dir.join("mtc.app");
        bundle(&target, "0.1.0");
        // Nothing to move in: the rename fails after the old one moved aside.
        let missing = dir.join(".staging/mtc.app");

        assert!(swap(&target, &missing).is_err());

        assert_eq!(version_at(&target), "0.1.0");
        assert!(!dir.join(".mtc.app.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    /// Serve `routes` (path → body) over HTTP on this computer; anything
    /// else is a 404. Returns the base URL.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> String {
        use std::io::{BufRead as _, BufReader, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                reader.read_line(&mut line).unwrap();
                // Skip the headers.
                let mut header = String::new();
                while reader.read_line(&mut header).unwrap() > 2 {
                    header.clear();
                }
                let path = line.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = match routes.iter().find(|(p, _)| *p == path) {
                    Some((_, body)) => ("200 OK", body.clone()),
                    None => ("404 Not Found", b"Not Found".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        base
    }

    #[test]
    fn reads_the_feed_and_downloads_a_verified_archive() {
        let archive = b"not really a zip".to_vec();
        let sha256: String = Sha256::digest(&archive)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let downloads = serve(vec![("/archive.zip", archive.clone())]);
        let feed = format!(
            r#"{{"version": "9.0.0", "page": "{downloads}/page",
                "downloads": {{"macos-universal": {{"url": "{downloads}/archive.zip", "sha256": "{sha256}"}}}}}}"#
        );
        let feed = serve(vec![("/latest.json", feed.into_bytes())]);

        let manifest = futures::executor::block_on(fetch_manifest(format!("{feed}/latest.json")))
            .unwrap()
            .expect("the feed has a release");
        assert_eq!(manifest.version, Version::new(9, 0, 0));
        let download = manifest.downloads["macos-universal"].clone();
        let bytes = futures::executor::block_on(super::download(download.clone())).unwrap();
        assert_eq!(bytes, archive);

        let damaged = Download {
            sha256: "0".repeat(64),
            ..download
        };
        assert!(futures::executor::block_on(super::download(damaged)).is_err());
    }

    #[test]
    fn a_missing_feed_means_no_release_yet() {
        let base = serve(Vec::new());
        let feed = futures::executor::block_on(fetch_manifest(format!("{base}/latest.json")));
        assert_eq!(feed.unwrap(), None);
    }

    #[test]
    fn checksums_must_match() {
        let digest = Sha256::digest(b"test");
        let hex = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert!(verify(&digest, hex).is_ok());
        assert!(verify(&digest, &hex.to_uppercase()).is_ok());
        assert!(verify(&Sha256::digest(b"tset"), hex).is_err());
    }
}
