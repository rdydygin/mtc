//! Fetching the release feed, downloading a release, and putting it in place
//! of the running app bundle.
//!
//! Network calls run on the Mattermost client's I/O runtime (see
//! [`mattermost::runtime`]); the file work is blocking and runs off the UI
//! thread on that runtime's blocking pool.

use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use semver::Version;
use sha2::{Digest as _, Sha256};

use super::manifest::{Download, Manifest};

/// No release comes close; a response this large is not one of ours.
const MAX_ARCHIVE_BYTES: usize = 512 * 1024 * 1024;
/// The feed is a few hundred bytes of JSON.
const MAX_FEED_BYTES: usize = 1024 * 1024;

fn http(source: &str) -> Result<reqwest::Client> {
    let source = url::Url::parse(source).context("invalid update URL")?;
    ensure!(
        super::manifest::is_secure(source.as_str(), cfg!(debug_assertions)),
        "updates require HTTPS or loopback HTTP"
    );
    reqwest::Client::builder()
        .user_agent(concat!("mtc/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many update redirects")
            } else if super::manifest::allowed_url(
                attempt.previous().last().unwrap_or(&source),
                attempt.url(),
                cfg!(debug_assertions),
            ) {
                attempt.follow()
            } else {
                attempt
                    .error("update redirect requires HTTPS (loopback HTTP only for debug testing)")
            }
        }))
        .connect_timeout(Duration::from_secs(30))
        // Silence, not size: a large download on a slow link still finishes.
        .read_timeout(Duration::from_secs(60))
        .build()
        .context("failed to build the HTTP client")
}

/// The newest release, or `None` when the feed has none yet.
pub async fn fetch_manifest(url: String) -> Result<Option<Manifest>> {
    mattermost::runtime::run(async move {
        let response = http(&url)?
            .get(&url)
            .send()
            .await
            .context("couldn’t reach the release feed")?;
        // Releases published before the feed existed have no `latest.json`.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        // The final feed's transport determines whether HTTP downloads are safe.
        // A local debug feed may have redirected to an HTTPS manifest.
        let feed_url = response.url().clone();
        let response = response
            .error_for_status()
            .context("the release feed is unavailable")?;
        let bytes = read_bounded(
            response,
            MAX_FEED_BYTES,
            "the release feed is too large",
            "couldn’t read the release feed",
        )
        .await?;
        let manifest = Manifest::parse(&bytes)?;
        manifest.validate_downloads(&feed_url)?;
        Ok(Some(manifest))
    })
    .await
}

/// Download a release archive and check it against the feed's checksum.
pub async fn download(download: Download) -> Result<Vec<u8>> {
    mattermost::runtime::run(async move {
        let response = http(&download.url)?
            .get(&download.url)
            .send()
            .await
            .context("couldn’t download the update")?
            .error_for_status()
            .context("couldn’t download the update")?;
        let bytes = read_bounded(
            response,
            MAX_ARCHIVE_BYTES,
            "the update is too large",
            "the update download was interrupted",
        )
        .await?;
        verify(&Sha256::digest(&bytes), &download.sha256)?;
        Ok(bytes)
    })
    .await
}

/// Read `response`'s body, failing with `too_large` as soon as its declared
/// length, or the bytes decoded so far, exceed `limit`.
async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
    too_large: &'static str,
    interrupted: &'static str,
) -> Result<Vec<u8>> {
    ensure!(
        response
            .content_length()
            .is_none_or(|len| len <= limit as u64),
        too_large
    );
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.context(interrupted)? {
        ensure!(chunk.len() <= limit - bytes.len(), too_large);
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn verify(digest: &[u8], expected: &str) -> Result<()> {
    let actual: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    ensure!(
        actual.eq_ignore_ascii_case(expected),
        "the update was damaged in transit (checksum mismatch)"
    );
    Ok(())
}

/// Check eligibility off the UI thread. A symlink is never an install target.
/// The installer repeats this check while holding the installation lock.
pub async fn installable_target(target: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "macos")]
    return mattermost::runtime::run(async move {
        tokio::task::spawn_blocking(move || target.filter(|path| replacement::is_installable(path)))
            .await
            .context("checking update eligibility was cancelled")
    })
    .await
    .ok()
    .flatten();
    #[cfg(not(target_os = "macos"))]
    target
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
        use super::replacement;
        let parent = target.parent().context("the app has no enclosing folder")?;
        let _lock = replacement::lock(parent)?
            .context("another mtc instance is installing an update; skipping this round")?;
        ensure!(
            replacement::is_installable(target),
            "mtc can’t replace a missing or symlinked app"
        );
        // Private, unique staging beside the app keeps the swap on one volume.
        let staging = replacement::create_staging(parent, version)?;
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
            Ok(app)
        })();
        let app = match result {
            Ok(app) => app,
            Err(err) => {
                replacement::remove_best_effort(&staging);
                return Err(err);
            }
        };
        let result = replacement::swap(target, &app);
        if result.is_ok() {
            replacement::remove_best_effort(&staging);
        } else {
            // A fallback rollback may have failed. Retain our transaction,
            // report its location, and never promote it on a later check.
            log::warn!("update staging preserved at {}", staging.display());
        }
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

#[cfg(any(target_os = "macos", test))]
mod replacement {
    use std::{
        fs, io,
        path::{Path, PathBuf},
    };

    use anyhow::{Context as _, Result, ensure};

    pub(super) fn is_installable(target: &Path) -> bool {
        fs::symlink_metadata(target).is_ok_and(|metadata| metadata.file_type().is_dir())
    }

    pub(super) fn create_staging(parent: &Path, version: &semver::Version) -> Result<PathBuf> {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).context("couldn’t choose an update staging name")?;
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = parent.join(format!(".mtc-update-{version}-{suffix}"));
        let builder = fs::DirBuilder::new();
        #[cfg(unix)]
        let builder = {
            use std::os::unix::fs::DirBuilderExt as _;
            let mut builder = builder;
            builder.mode(0o700);
            builder
        };
        builder
            .create(&path)
            .context("couldn’t create update staging")?;
        Ok(path)
    }

    /// Keep the lock file in place: unlinking it would let instances lock
    /// different inodes. Closing the file releases flock, including on exit.
    #[cfg(target_os = "macos")]
    pub(super) fn lock(parent: &Path) -> Result<Option<fs::File>> {
        use std::os::{fd::AsRawFd as _, unix::fs::OpenOptionsExt as _};
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(parent.join(".mtc-update.lock"))
            .context("couldn’t open the update lock")?;
        ensure!(
            file.metadata()?.is_file(),
            "the update lock is not a regular file"
        );
        // SAFETY: file owns this valid descriptor throughout the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(Some(file));
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(err).context("couldn’t lock the update installation")
        }
    }

    pub(super) fn remove_best_effort(path: &Path) {
        if let Err(err) = fs::remove_dir_all(path)
            && err.kind() != io::ErrorKind::NotFound
        {
            log::warn!("removing {}: {err}", path.display());
        }
    }

    #[cfg(target_os = "macos")]
    fn exchange(target: &Path, new: &Path) -> io::Result<()> {
        use std::{ffi::CString, os::unix::ffi::OsStrExt as _};
        let target = CString::new(target.as_os_str().as_bytes())?;
        let new = CString::new(new.as_os_str().as_bytes())?;
        // SAFETY: both pointers are valid NUL-terminated paths for the call.
        // Staging is beside the installation, on the same volume.
        if unsafe { libc::renamex_np(new.as_ptr(), target.as_ptr(), libc::RENAME_SWAP) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn exchange(_: &Path, _: &Path) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn swap(target: &Path, new: &Path) -> Result<()> {
        swap_with(target, new, exchange)
    }

    fn swap_with(
        target: &Path,
        new: &Path,
        exchange: impl FnOnce(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        ensure!(
            is_installable(target),
            "mtc can’t replace a missing or symlinked app"
        );
        ensure!(
            is_installable(new),
            "the staged app is missing or symlinked"
        );
        match exchange(target, new) {
            Ok(()) => {
                // After exchange, `new` holds the previous installation. All
                // verification has already finished; do not verify this path.
                remove_best_effort(new);
                Ok(())
            }
            Err(err) if err.kind() == io::ErrorKind::Unsupported => {
                fallback(target, new, |from, to| fs::rename(from, to))
            }
            Err(err) => Err(err).context("couldn’t exchange the app bundles"),
        }
    }

    /// A fallback backup belongs to this transaction's unique staging directory.
    /// A rollback failure leaves it there for manual recovery only.
    fn fallback(
        target: &Path,
        new: &Path,
        mut rename: impl FnMut(&Path, &Path) -> io::Result<()>,
    ) -> Result<()> {
        let aside = new.with_file_name("previous.app");
        // symlink_metadata also notices dangling symlinks. Never overwrite one.
        match fs::symlink_metadata(&aside) {
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            _ => anyhow::bail!("the transaction backup path is already occupied"),
        }
        rename(target, &aside)
            .with_context(|| format!("mtc can’t replace {}", target.display()))?;
        if let Err(err) = rename(new, target) {
            if let Err(rollback) = rename(&aside, target) {
                anyhow::bail!(
                    "couldn’t install mtc: {err}; couldn’t restore it: {rollback}; previous app preserved at {}",
                    aside.display()
                );
            }
            return Err(err).context("couldn’t install mtc; previous app restored");
        }
        remove_best_effort(&aside);
        Ok(())
    }

    #[cfg(all(test, target_os = "macos"))]
    #[test]
    fn native_exchange_keeps_both_bundles_until_cleanup() {
        let dir = std::env::temp_dir().join(format!("mtc-native-swap-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("mtc.app");
        let new = dir.join("staged.app");
        fs::create_dir(&target).unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(target.join("version"), "old").unwrap();
        fs::write(new.join("version"), "new").unwrap();
        exchange(&target, &new).unwrap();
        assert_eq!(fs::read_to_string(target.join("version")).unwrap(), "new");
        assert_eq!(fs::read_to_string(new.join("version")).unwrap(), "old");
        remove_best_effort(&new);
        assert!(target.is_dir());
        assert!(!new.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(test)]
    pub(super) fn unsupported_swap(target: &Path, new: &Path) -> Result<()> {
        swap_with(target, new, |_, _| Err(io::ErrorKind::Unsupported.into()))
    }

    #[cfg(test)]
    pub(super) fn fail_exchange(target: &Path, new: &Path) -> Result<()> {
        swap_with(target, new, |_, _| {
            Err(io::ErrorKind::PermissionDenied.into())
        })
    }

    #[cfg(test)]
    pub(super) fn fail_install(target: &Path, new: &Path) -> Result<()> {
        let mut calls = 0;
        fallback(target, new, |from, to| {
            calls += 1;
            if calls == 2 {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                fs::rename(from, to)
            }
        })
    }

    #[cfg(test)]
    pub(super) fn fail_rollback(target: &Path, new: &Path) -> Result<()> {
        let mut calls = 0;
        fallback(target, new, |from, to| {
            calls += 1;
            if calls == 1 {
                fs::rename(from, to)
            } else {
                Err(io::ErrorKind::PermissionDenied.into())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::replacement::{
        create_staging, fail_exchange, fail_install, fail_rollback, swap, unsupported_swap,
    };

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
        // Nothing to move in: exchange fails without moving the installed app.
        let missing = dir.join(".staging/mtc.app");

        assert!(swap(&target, &missing).is_err());

        assert_eq!(version_at(&target), "0.1.0");
        assert!(!dir.join(".mtc.app.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unsupported_exchange_falls_back_and_rolls_back_on_failure() {
        let dir = scratch("unsupported");
        let target = dir.join("mtc.app");
        let new = dir.join(".staging/mtc.app");
        bundle(&target, "0.1.0");
        fs::create_dir_all(new.parent().unwrap()).unwrap();
        assert!(unsupported_swap(&target, &new).is_err());
        assert_eq!(version_at(&target), "0.1.0");
        assert!(!dir.join(".mtc.app.old").exists());
        bundle(&new, "0.2.0");
        let err = fail_install(&target, &new).unwrap_err();
        assert!(err.to_string().contains("previous app restored"));
        assert_eq!(version_at(&target), "0.1.0");
        assert_eq!(version_at(&new), "0.2.0");
        assert!(!new.with_file_name("previous.app").exists());
        unsupported_swap(&target, &new).unwrap();
        assert_eq!(version_at(&target), "0.2.0");
        assert!(!new.exists());
        assert!(!dir.join(".mtc.app.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fallback_never_overwrites_an_existing_backup() {
        let dir = scratch("existing-backup");
        let target = dir.join("mtc.app");
        let new = dir.join(".staging/mtc.app");
        let aside = new.with_file_name("previous.app");
        bundle(&target, "0.0.9");
        bundle(&aside, "0.1.0");
        bundle(&new, "0.2.0");
        assert!(unsupported_swap(&target, &new).is_err());
        assert_eq!(version_at(&aside), "0.1.0");
        assert_eq!(version_at(&new), "0.2.0");
        assert_eq!(version_at(&target), "0.0.9");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn exchange_failure_does_not_start_a_fallback() {
        let dir = scratch("exchange-fail");
        let target = dir.join("mtc.app");
        let new = dir.join(".mtc-update-0.2.0/mtc.app");
        bundle(&target, "0.1.0");
        bundle(&new, "0.2.0");
        assert!(fail_exchange(&target, &new).is_err());
        assert_eq!(version_at(&target), "0.1.0");
        assert_eq!(version_at(&new), "0.2.0");
        assert!(!dir.join(".mtc.app.old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_rollback_reports_and_preserves_backup_without_promoting_it() {
        let dir = scratch("rollback-fail");
        let target = dir.join("mtc.app");
        let new = dir.join(".mtc-update-0.2.0/mtc.app");
        bundle(&target, "0.1.0");
        bundle(&new, "0.2.0");
        let err = fail_rollback(&target, &new).unwrap_err();
        assert!(err.to_string().contains("previous app preserved"));
        assert!(!target.exists());
        let aside = new.with_file_name("previous.app");
        assert_eq!(version_at(&aside), "0.1.0");
        assert!(err.to_string().contains(&aside.display().to_string()));
        assert_eq!(version_at(&new), "0.2.0");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unique_staging_leaves_unrelated_siblings_untouched() {
        let dir = scratch("staging");
        let target = dir.join("mtc.app");
        let old = dir.join(".mtc.app.old");
        let sibling = dir.join(".mtc-update-0.2.0/mtc.app");
        bundle(&target, "0.1.0");
        bundle(&old, "unverified");
        bundle(&sibling, "unrelated");
        let first = create_staging(&dir, &Version::new(0, 2, 0)).unwrap();
        let second = create_staging(&dir, &Version::new(0, 2, 0)).unwrap();
        assert_ne!(first, second);
        let new = first.join("mtc.app");
        bundle(&new, "0.2.0");
        swap(&target, &new).unwrap();
        replacement::remove_best_effort(&first);
        assert_eq!(version_at(&target), "0.2.0");
        assert_eq!(version_at(&old), "unverified");
        assert_eq!(version_at(&sibling), "unrelated");
        assert!(second.is_dir());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_install_path_is_refused_without_deleting_its_referent() {
        let dir = scratch("symlink-target");
        let target = dir.join("mtc.app");
        let new = dir.join(".staging/mtc.app");
        bundle(&new, "0.2.0");
        std::os::unix::fs::symlink(&new, &target).unwrap();
        assert!(!replacement::is_installable(&target));
        #[cfg(target_os = "macos")]
        assert!(futures::executor::block_on(installable_target(Some(target.clone()))).is_none());
        assert!(swap(&target, &new).is_err());
        assert_eq!(version_at(&new), "0.2.0");
        assert!(fs::symlink_metadata(&target).unwrap().is_symlink());
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn lock_contention_skips_installing_and_creating_staging() {
        if let Some(dir) = std::env::var_os("MTC_TEST_CONTENDED_UPDATE_LOCK") {
            assert!(replacement::lock(Path::new(&dir)).unwrap().is_none());
            return;
        }
        let dir = scratch("lock");
        let target = dir.join("mtc.app");
        bundle(&target, "0.1.0");
        let guard = replacement::lock(&dir).unwrap().unwrap();
        assert!(replacement::lock(&dir).unwrap().is_none());
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "update::install::tests::lock_contention_skips_installing_and_creating_staging",
            ])
            .env("MTC_TEST_CONTENDED_UPDATE_LOCK", &dir)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "{}",
            String::from_utf8_lossy(&child.stderr)
        );
        let err = macos::install(b"not an archive", &Version::new(0, 2, 0), &target).unwrap_err();
        assert!(err.to_string().contains("skipping this round"));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 2);
        assert_eq!(version_at(&target), "0.1.0");
        drop(guard);
        assert!(replacement::lock(&dir).unwrap().is_some());
        fs::remove_dir_all(dir).unwrap();
    }

    /// Serve `routes` (path → body) over HTTP on this computer; anything
    /// else is a 404. Returns the base URL.
    fn serve(routes: Vec<(&'static str, Vec<u8>)>) -> String {
        serve_with(|_| routes)
    }

    fn serve_with(routes: impl FnOnce(&str) -> Vec<(&'static str, Vec<u8>)>) -> String {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes = routes(&base);
        serve_each(listener, move |request, mut stream| {
            let path = request.split_whitespace().nth(1).unwrap_or_default();
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
        });
        base
    }

    /// Answer each connection on its own thread, once its request line and
    /// headers have arrived. A stray local connection that sends nothing
    /// times out on its own thread instead of holding up the test's request.
    fn serve_each(
        listener: std::net::TcpListener,
        answer: impl Fn(&str, std::net::TcpStream) + Send + Sync + 'static,
    ) {
        use std::io::{BufRead as _, BufReader};
        let answer = std::sync::Arc::new(answer);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let answer = answer.clone();
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                    let Ok(clone) = stream.try_clone() else {
                        return;
                    };
                    let mut reader = BufReader::new(clone);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                        header.clear();
                    }
                    answer(&line, stream);
                });
            }
        });
    }

    /// Opt-in network smoke test: only request archive headers, never its body.
    #[test]
    #[ignore = "requires network access to the published GitHub release"]
    fn live_official_feed_and_download_pass_the_url_policy() {
        let manifest = futures::executor::block_on(fetch_manifest(
            super::super::manifest::FEED_URL.to_owned(),
        ))
        .unwrap()
        .expect("the official feed must contain a release");
        let download = manifest
            .downloads
            .get("macos-universal")
            .expect("the official release must offer the macOS archive")
            .clone();
        futures::executor::block_on(mattermost::runtime::run(async move {
            let response = http(&download.url)?
                .head(&download.url)
                .send()
                .await?
                .error_for_status()?;
            ensure!(
                response.url().scheme() == "https",
                "archive must stay on HTTPS"
            );
            Ok(())
        }))
        .unwrap();
    }

    #[test]
    fn reads_the_feed_and_downloads_a_verified_archive() {
        let archive = b"not really a zip".to_vec();
        let sha256: String = Sha256::digest(&archive)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let feed = serve_with(|downloads| {
            let feed = format!(
                r#"{{"version": "9.0.0", "page": "{downloads}/page",
                "downloads": {{"macos-universal": {{"url": "{downloads}/archive.zip", "sha256": "{sha256}"}}}}}}"#
            );
            vec![
                ("/latest.json", feed.into_bytes()),
                ("/archive.zip", archive.clone()),
            ]
        });

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
    fn an_oversized_feed_is_refused() {
        let base = serve(vec![("/latest.json", vec![b' '; MAX_FEED_BYTES + 1])]);
        let feed = futures::executor::block_on(fetch_manifest(format!("{base}/latest.json")));
        assert_eq!(
            feed.unwrap_err().to_string(),
            "the release feed is too large"
        );
    }

    /// A declared length past the limit fails before any body arrives: this
    /// server sends headers only, then holds the connection open.
    #[test]
    fn an_oversized_declared_feed_is_refused_before_reading() {
        use std::io::Write as _;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://{addr}");
        serve_each(listener, |request, mut stream| {
            // Stray local connections get a 404, not the held response.
            if request.starts_with("GET /latest.json ") {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                    MAX_FEED_BYTES + 1
                );
                let _ = stream.write_all(head.as_bytes());
                // Hold the connection open without sending the body.
                std::thread::sleep(std::time::Duration::from_secs(20));
            } else {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        // An idle stray connection, accepted first, must not hold up the feed.
        let _stray = std::net::TcpStream::connect(addr).unwrap();
        let started = std::time::Instant::now();
        let feed = futures::executor::block_on(fetch_manifest(format!("{base}/latest.json")));
        assert_eq!(
            feed.unwrap_err().to_string(),
            "the release feed is too large"
        );
        // Well within the 60 s read timeout a body-less read would wait out.
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
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
