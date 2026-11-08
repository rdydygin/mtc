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
        for download in manifest.downloads.values() {
            if !is_sha256(&download.sha256) {
                bail!("release feed has a malformed checksum");
            }
            if !is_secure(&download.url) {
                bail!("release feed offers a download that isn’t over HTTPS");
            }
        }
        Ok(manifest)
    }
}

/// HTTPS, or plain HTTP to this computer, for trying an update locally.
fn is_secure(url: &str) -> bool {
    let Ok(url) = url::Url::parse(url) else {
        return false;
    };
    match (url.scheme(), url.host()) {
        ("https", _) => true,
        ("http", Some(url::Host::Domain(name))) => name == "localhost",
        ("http", Some(url::Host::Ipv4(ip))) => ip.is_loopback(),
        ("http", Some(url::Host::Ipv6(ip))) => ip.is_loopback(),
        _ => false,
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
        assert!(Manifest::parse(local.as_bytes()).is_ok());
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
