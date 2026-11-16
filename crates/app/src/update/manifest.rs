//! What a release says about itself, and whether this build should take it.
//!
//! Each GitHub release carries a `latest.json` asset (written by
//! `.github/workflows/release.yml`). GitHub redirects
//! `releases/latest/download/latest.json` to the newest published release's
//! copy, so one fixed URL always describes the current release. Asset
//! downloads aren't subject to the API's per-address rate limit, which a
//! whole office behind one address could otherwise exhaust.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use semver::Version;
use serde::Deserialize;

/// The release feed of the official builds.
pub const FEED_URL: &str = "https://github.com/rdydygin/mtc/releases/latest/download/latest.json";

/// The archive name this build installs from, if it can install itself.
#[cfg(target_os = "macos")]
pub const PLATFORM: Option<&str> = Some("macos-universal");
#[cfg(not(target_os = "macos"))]
pub const PLATFORM: Option<&str> = None;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Manifest {
    pub version: Version,
    /// The release's page, with its notes and every download.
    pub page: String,
    #[serde(default)]
    pub downloads: std::collections::BTreeMap<String, Download>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Download {
    pub url: String,
    pub sha256: String,
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes).context("unreadable release feed")?;
        // The page is handed to the system's URL opener when the person
        // clicks Download, so a `file:` path or another app's scheme must
        // never get that far.
        if !is_secure(&manifest.page, cfg!(debug_assertions)) {
            bail!("release feed links to a release page that isn’t over HTTPS");
        }
        for download in manifest.downloads.values() {
            if !is_sha256(&download.sha256) {
                bail!("release feed has a malformed checksum");
            }
            if !is_secure(&download.url, cfg!(debug_assertions)) {
                bail!("release feed offers a download that isn’t over HTTPS");
            }
        }
        Ok(manifest)
    }
}

/// HTTPS, or plain HTTP to this computer, for trying an update locally.
pub(super) fn is_secure(url: &str, allow_loopback: bool) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    if !url.username().is_empty() || url.password().is_some() {
        return false;
    }
    match (url.scheme(), url.host()) {
        ("https", Some(_)) => true,
        ("http", Some(url::Host::Domain(name))) => allow_loopback && name == "localhost",
        ("http", Some(url::Host::Ipv4(ip))) => allow_loopback && ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => allow_loopback && ip.is_loopback(),
        _ => false,
    }
}

/// Overrides are a development tool, never a release trust root.
pub(super) fn validate_override(feed: &str, debug: bool) -> Result<()> {
    anyhow::ensure!(debug, "release builds always use the official feed");
    anyhow::ensure!(
        is_secure(feed, debug),
        "the feed must use HTTPS or loopback HTTP"
    );
    Ok(())
}

/// Hosts may change as GitHub moves assets between CDNs. Trust the HTTPS
/// feed and its archive checksum, rather than a list of CDN hostnames.
/// A secure source can never downgrade, including to loopback HTTP.
pub(super) fn allowed_url(source: &url::Url, target: &url::Url, debug: bool) -> bool {
    is_secure(target.as_str(), debug && source.scheme() == "http")
}

impl Manifest {
    pub(super) fn validate_downloads(&self, feed: &url::Url) -> Result<()> {
        let page = url::Url::parse(&self.page)?;
        anyhow::ensure!(
            allowed_url(feed, &page, cfg!(debug_assertions)),
            "release pages require HTTPS (loopback HTTP only for debug testing)"
        );
        for download in self.downloads.values() {
            let target = url::Url::parse(&download.url)?;
            anyhow::ensure!(
                allowed_url(feed, &target, cfg!(debug_assertions)),
                "release downloads require HTTPS (loopback HTTP only for debug testing)"
            );
        }
        Ok(())
    }
}

fn is_sha256(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The version of this build.
pub fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the package version is semver")
}

/// What to do about a release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// This build is the release, or newer.
    UpToDate,
    /// Download the archive and replace the app at `target`.
    Install {
        version: Version,
        page: String,
        download: Download,
        target: PathBuf,
    },
    /// A newer version exists, but this copy can't replace itself: it isn't
    /// an app bundle, sits somewhere it can't write, or has no build for
    /// this platform. Point to the release page instead.
    Offer { version: Version, page: String },
}

/// Decide what to do about `manifest`, for a build at `current` that could
/// replace the app bundle at `installable` (when it can replace itself).
pub fn plan(manifest: &Manifest, current: &Version, installable: Option<&Path>) -> Plan {
    // Pre-releases are only ever offered to pre-release builds.
    if manifest.version <= *current || (!manifest.version.pre.is_empty() && current.pre.is_empty())
    {
        return Plan::UpToDate;
    }
    let download = PLATFORM.and_then(|platform| manifest.downloads.get(platform));
    match (download, installable) {
        (Some(download), Some(target)) => Plan::Install {
            version: manifest.version.clone(),
            page: manifest.page.clone(),
            download: download.clone(),
            target: target.to_owned(),
        },
        _ => Plan::Offer {
            version: manifest.version.clone(),
            page: manifest.page.clone(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FEED: &str = r#"{
        "version": "0.2.0",
        "page": "https://github.com/rdydygin/mtc/releases/tag/v0.2.0",
        "downloads": {
            "macos-universal": {
                "url": "https://github.com/rdydygin/mtc/releases/download/v0.2.0/mtc-v0.2.0-macos-universal.zip",
                "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
            }
        }
    }"#;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn feed_override_policy_table() {
        for (feed, debug_ok) in [
            (FEED_URL, true),
            ("https://example.com/latest.json", true),
            ("http://localhost:8000/latest.json", true),
            ("http://127.0.0.2:8000/latest.json", true),
            ("http://[::1]:8000/latest.json", true),
            ("http://example.com/latest.json", false),
            ("http://localhost.evil/latest.json", false),
            ("http://192.168.1.2/latest.json", false),
            ("file:///tmp/latest.json", false),
            ("garbage", false),
        ] {
            assert_eq!(validate_override(feed, true).is_ok(), debug_ok, "{feed}");
            assert!(validate_override(feed, false).is_err(), "{feed}");
        }
    }

    #[test]
    fn download_and_redirect_policy_table() {
        for (source, target, debug, allowed) in [
            (
                FEED_URL,
                "https://release-assets.githubusercontent.com/archive.zip",
                false,
                true,
            ),
            (
                FEED_URL,
                "https://any-other-cdn.example/archive.zip",
                false,
                true,
            ),
            (FEED_URL, "https://example.com:444/archive.zip", false, true),
            (FEED_URL, "http://example.com/archive.zip", false, false),
            (FEED_URL, "http://127.0.0.1/archive.zip", true, false),
            (
                FEED_URL,
                "https://user:password@example.com/archive.zip",
                false,
                false,
            ),
            (FEED_URL, "ftp://example.com/archive.zip", false, false),
            (FEED_URL, "file:///tmp/archive.zip", false, false),
            (
                "http://127.0.0.1:8000/feed",
                "http://localhost:8001/archive.zip",
                true,
                true,
            ),
            (
                "http://127.0.0.1:8000/feed",
                "http://[::1]/archive.zip",
                true,
                true,
            ),
            (
                "http://127.0.0.1:8000/feed",
                "http://localhost/archive.zip",
                false,
                false,
            ),
            (
                "http://127.0.0.1:8000/feed",
                "http://example.com/archive.zip",
                true,
                false,
            ),
            (
                "http://127.0.0.1:8000/feed",
                "https://example.com/archive.zip",
                true,
                true,
            ),
            (
                "https://example.com/redirect",
                "http://localhost/archive.zip",
                true,
                false,
            ),
        ] {
            assert_eq!(
                allowed_url(
                    &url::Url::parse(source).unwrap(),
                    &url::Url::parse(target).unwrap(),
                    debug
                ),
                allowed,
                "{source} -> {target}, debug={debug}"
            );
        }
        let feed = url::Url::parse(FEED_URL).unwrap();
        for (target, allowed) in [
            (
                "https://release-assets.githubusercontent.com/archive.zip",
                true,
            ),
            ("https://arbitrary.example/archive.zip", true),
            ("http://127.0.0.1/archive.zip", false),
            ("http://example.com/archive.zip", false),
            ("ftp://example.com/archive.zip", false),
        ] {
            let mut manifest = Manifest::parse(FEED.as_bytes()).unwrap();
            manifest.downloads.get_mut("macos-universal").unwrap().url = target.into();
            assert_eq!(
                manifest.validate_downloads(&feed).is_ok(),
                allowed,
                "{target}"
            );
        }
    }

    #[test]
    fn release_pages_must_be_web_pages() {
        let official = "https://github.com/rdydygin/mtc/releases/tag/v0.2.0";
        for (page, allowed) in [
            (official, true),
            ("file:///Applications/Calculator.app", false),
            ("file:///tmp/evil.command", false),
            ("javascript:alert(1)", false),
            ("mattermost-dev://callback", false),
            ("x-apple.systempreferences:com.apple.preference", false),
            ("smb://attacker.example/share", false),
            ("http://example.com/release", false),
            ("https://user:password@example.com/release", false),
            ("/Applications/Calculator.app", false),
        ] {
            let feed = FEED.replace(official, page);
            assert_eq!(Manifest::parse(feed.as_bytes()).is_ok(), allowed, "{page}");
        }
        // A loopback page is only for a loopback debug feed, never an HTTPS one.
        let mut manifest = Manifest::parse(FEED.as_bytes()).unwrap();
        manifest.page = "http://127.0.0.1/page".into();
        assert!(
            manifest
                .validate_downloads(&url::Url::parse(FEED_URL).unwrap())
                .is_err()
        );
    }

    #[test]
    fn redirected_https_manifest_rejects_a_loopback_download() {
        let mut manifest = Manifest::parse(FEED.as_bytes()).unwrap();
        manifest.downloads.get_mut("macos-universal").unwrap().url =
            "http://127.0.0.1/archive.zip".into();
        let original = url::Url::parse("http://127.0.0.1/latest.json").unwrap();
        let final_url = url::Url::parse("https://cdn.example/latest.json").unwrap();
        assert_eq!(
            manifest.validate_downloads(&original).is_ok(),
            cfg!(debug_assertions)
        );
        assert!(manifest.validate_downloads(&final_url).is_err());
    }

    #[test]
    fn parses_the_feed_the_release_workflow_writes() {
        let manifest = Manifest::parse(FEED.as_bytes()).unwrap();
        assert_eq!(manifest.version, v("0.2.0"));
        assert_eq!(
            manifest.downloads["macos-universal"].sha256,
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        );
    }

    #[test]
    fn rejects_malformed_checksums_and_plain_http() {
        let short = FEED.replace("9f86d081884c7d65", "9f86");
        assert!(Manifest::parse(short.as_bytes()).is_err());
        let plain = FEED.replace(
            "https://github.com/rdydygin/mtc/releases/download",
            "http://x",
        );
        assert!(Manifest::parse(plain.as_bytes()).is_err());
        // Plain HTTP only to this computer, for trying an update locally.
        let local = FEED.replace(
            "https://github.com/rdydygin/mtc/releases/download",
            "http://127.0.0.1:8000",
        );
        assert_eq!(
            Manifest::parse(local.as_bytes()).is_ok(),
            cfg!(debug_assertions)
        );
        assert!(Manifest::parse(b"<html>Not Found</html>").is_err());
    }

    #[test]
    fn same_or_older_releases_are_up_to_date() {
        let manifest = Manifest::parse(FEED.as_bytes()).unwrap();
        let app = Path::new("/Applications/mtc.app");
        assert_eq!(plan(&manifest, &v("0.2.0"), Some(app)), Plan::UpToDate);
        assert_eq!(plan(&manifest, &v("0.10.0"), Some(app)), Plan::UpToDate);
    }

    #[test]
    fn versions_compare_numerically() {
        let manifest = Manifest::parse(FEED.replace("0.2.0", "0.10.0").as_bytes()).unwrap();
        assert_ne!(plan(&manifest, &v("0.9.0"), None), Plan::UpToDate);
    }

    #[test]
    fn prereleases_reach_only_prerelease_builds() {
        let manifest =
            Manifest::parse(FEED.replace("\"0.2.0\"", "\"0.2.0-beta.1\"").as_bytes()).unwrap();
        assert_eq!(plan(&manifest, &v("0.1.0"), None), Plan::UpToDate);
        assert_ne!(plan(&manifest, &v("0.2.0-alpha.3"), None), Plan::UpToDate);
    }

    #[test]
    fn installs_when_it_can_and_offers_the_page_otherwise() {
        let manifest = Manifest::parse(FEED.as_bytes()).unwrap();
        let page = Plan::Offer {
            version: v("0.2.0"),
            page: manifest.page.clone(),
        };
        assert_eq!(plan(&manifest, &v("0.1.0"), None), page);
        let app = Path::new("/Applications/mtc.app");
        if PLATFORM.is_some() {
            assert!(matches!(
                plan(&manifest, &v("0.1.0"), Some(app)),
                Plan::Install { target, .. } if target == app
            ));
        } else {
            assert_eq!(plan(&manifest, &v("0.1.0"), Some(app)), page);
        }
        // A release without a build for this platform: only the page.
        let mut bare = manifest.clone();
        bare.downloads.clear();
        assert_eq!(plan(&bare, &v("0.1.0"), Some(app)), page);
    }
}
