//! Automatic updates: finds new releases, installs them in the background,
//! and offers a restart.
//!
//! [`Updater`] owns the update state for the whole app and lives in a GPUI
//! global. A few seconds after launch, and every few hours after that, it
//! reads the release feed (see [`manifest`]). When a newer release has a
//! build for this platform and mtc runs from an app bundle it can write to,
//! the release is downloaded, checked against the feed's checksum, and put in
//! place of the bundle; it takes effect on the next launch, and the title bar
//! offers a restart. Otherwise mtc says a new version is out and links to it.
//!
//! Only macOS builds are published, so Linux and Windows only hear about
//! new versions. Debug builds don't check on their own unless
//! `MTC_UPDATE_FEED` names a feed, so `cargo run` stays quiet.

mod install;
mod manifest;

use std::{
    path::PathBuf,
    time::{Duration, SystemTime},
};

use gpui_kit::component::{
    Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    notification::Notification,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, Global, IntoElement as _,
    SharedString, Task, Window,
};
use semver::Version;

use self::manifest::{Plan, current_version};
use crate::persist::{self, Updates};

/// Leave launch to the startup requests first.
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(10);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
/// How often the schedule wakes to see whether a check is due. Timers pause
/// while the computer sleeps, so the interval is measured on the wall clock.
const TICK: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    Idle,
    Checking,
    Downloading {
        version: Version,
    },
    /// Installed; the next launch runs it.
    Ready {
        version: Version,
    },
    /// A newer version to download by hand.
    Available {
        version: Version,
        page: SharedString,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateEvent {
    Ready {
        version: Version,
    },
    Available {
        version: Version,
        page: SharedString,
        /// Why it wasn't installed, when installing was tried and failed.
        problem: Option<SharedString>,
    },
    /// Only for a check that was asked for.
    UpToDate,
    /// Only for a check that was asked for.
    Failed {
        message: SharedString,
    },
}

pub struct Updater {
    status: UpdateStatus,
    feed: String,
    /// The running check was asked for, so its outcome is reported even
    /// when there is nothing new.
    manual: bool,
    last_check: Option<SystemTime>,
    /// When a check last reached the feed.
    checked_at: Option<SystemTime>,
    /// Why the last check failed, until the next one starts.
    error: Option<SharedString>,
    /// The newest version already announced, so later automatic checks
    /// don't announce it again.
    announced: Option<Version>,
    _check: Option<Task<()>>,
    _schedule: Task<()>,
}

impl EventEmitter<UpdateEvent> for Updater {}

struct GlobalUpdater(Entity<Updater>);

impl Global for GlobalUpdater {}

pub fn init(cx: &mut App) {
    let updater = cx.new(Updater::new);
    cx.set_global(GlobalUpdater(updater));
}

pub fn updater(cx: &App) -> Entity<Updater> {
    cx.global::<GlobalUpdater>().0.clone()
}

pub fn status(cx: &App) -> UpdateStatus {
    cx.try_global::<GlobalUpdater>()
        .map(|updater| updater.0.read(cx).status.clone())
        .unwrap_or(UpdateStatus::Idle)
}

/// Check now, and report the outcome even when there is nothing new.
pub fn check(cx: &mut App) {
    updater(cx).update(cx, |updater, cx| updater.check(true, cx));
}

/// Quit and open the installed update.
pub fn restart(cx: &mut App) {
    if matches!(status(cx), UpdateStatus::Ready { .. }) {
        persist::flush();
        cx.restart();
    }
}

pub fn set_automatic(automatic: bool, cx: &mut App) {
    persist::update(|state| {
        state.updates = if automatic {
            Updates::Automatic
        } else {
            Updates::Manual
        }
    });
    crate::actions::set_menus(cx);
    // Settings and ⌘K read the preference while rendering.
    cx.refresh_windows();
}

/// The version of this build, such as `0.1.0`.
pub fn version() -> Version {
    current_version()
}

/// Whether mtc checks for updates on its own.
pub fn is_automatic() -> bool {
    persist::read_with(|state| state.updates) == Updates::Automatic
}

/// The feed to read: `MTC_UPDATE_FEED`, for trying an update from a fork or
/// a local server, or the official releases.
fn feed_override() -> Option<String> {
    std::env::var("MTC_UPDATE_FEED")
        .ok()
        .filter(|feed| !feed.is_empty())
}

/// Whether this copy can install updates itself, rather than only say a new
/// version is out.
pub fn can_install(cx: &App) -> bool {
    installable_bundle(cx).is_some()
}

/// The app bundle this copy runs from, when it can replace itself.
fn installable_bundle(cx: &App) -> Option<PathBuf> {
    // Without a build for this platform there is nothing to install.
    manifest::PLATFORM?;
    // GPUI's test platform doesn't implement `app_path`, and a test
    // never runs from an installed bundle anyway.
    if cfg!(test) {
        return None;
    }
    let path = cx.app_path().ok()?;
    // macOS runs an app opened straight from a download from a read-only
    // copy ("App Translocation") until it is moved, e.g. to Applications.
    let translocated = path
        .components()
        .any(|part| part.as_os_str() == "AppTranslocation");
    (path.extension()? == "app" && !translocated).then_some(path)
}

impl Updater {
    fn new(cx: &mut Context<Self>) -> Self {
        let schedule = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_CHECK_DELAY).await;
            loop {
                let alive = this.update(cx, |this, cx| {
                    if this.is_due() {
                        this.check(false, cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(TICK).await;
            }
        });
        Self {
            status: UpdateStatus::Idle,
            feed: feed_override().unwrap_or_else(|| manifest::FEED_URL.to_owned()),
            manual: false,
            last_check: None,
            checked_at: None,
            error: None,
            announced: None,
            _check: None,
            _schedule: schedule,
        }
    }

    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    /// When a check last reached the release feed, whatever it found.
    pub fn checked_at(&self) -> Option<SystemTime> {
        self.checked_at
    }

    /// Why the last check failed, if it did.
    pub fn error(&self) -> Option<&SharedString> {
        self.error.as_ref()
    }

    fn is_due(&self) -> bool {
        let enabled = is_automatic() && (!cfg!(debug_assertions) || feed_override().is_some());
        let idle = matches!(
            self.status,
            UpdateStatus::Idle | UpdateStatus::Available { .. }
        );
        let stale = self.last_check.is_none_or(|at| {
            // A clock set back counts as due.
            at.elapsed()
                .map_or(true, |elapsed| elapsed >= CHECK_INTERVAL)
        });
        enabled && idle && stale
    }

    pub fn check(&mut self, manual: bool, cx: &mut Context<Self>) {
        match &self.status {
            UpdateStatus::Checking | UpdateStatus::Downloading { .. } => {
                // Already on it; report this one when it finishes.
                self.manual |= manual;
                return;
            }
            UpdateStatus::Ready { version } => {
                if manual {
                    cx.emit(UpdateEvent::Ready {
                        version: version.clone(),
                    });
                }
                return;
            }
            UpdateStatus::Idle | UpdateStatus::Available { .. } => {}
        }
        self.manual = manual;
        self.last_check = Some(SystemTime::now());
        self.error = None;
        let previous = std::mem::replace(&mut self.status, UpdateStatus::Checking);
        cx.notify();

        let feed = self.feed.clone();
        let installable = installable_bundle(cx);
        self._check = Some(cx.spawn(async move |this, cx| {
            let installable = install::installable_target(installable).await;
            let plan = match install::fetch_manifest(feed).await {
                Ok(Some(manifest)) => {
                    manifest::plan(&manifest, &current_version(), installable.as_deref())
                }
                Ok(None) => Plan::UpToDate,
                Err(err) => {
                    let _ = this.update(cx, |this, cx| this.failed(previous, err, cx));
                    return;
                }
            };
            match plan {
                Plan::UpToDate => {
                    let _ = this.update(cx, |this, cx| this.up_to_date(cx));
                }
                Plan::Offer { version, page } => {
                    let _ = this.update(cx, |this, cx| this.offer(version, page, None, cx));
                }
                Plan::Install {
                    version,
                    page,
                    download,
                    target,
                } => {
                    let _ = this.update(cx, |this, cx| {
                        this.status = UpdateStatus::Downloading {
                            version: version.clone(),
                        };
                        cx.notify();
                    });
                    let result = async {
                        let archive = install::download(download).await?;
                        install::install(archive, version.clone(), target).await
                    }
                    .await;
                    let _ = this.update(cx, |this, cx| match result {
                        Ok(()) => this.installed(version, cx),
                        Err(err) => {
                            log::warn!("installing mtc {version}: {err:#}");
                            let problem = sentence(&err).into();
                            this.offer(version, page, Some(problem), cx);
                        }
                    });
                }
            }
        }));
    }

    fn up_to_date(&mut self, cx: &mut Context<Self>) {
        self.status = UpdateStatus::Idle;
        self.checked_at = Some(SystemTime::now());
        if std::mem::take(&mut self.manual) {
            cx.emit(UpdateEvent::UpToDate);
        }
        cx.notify();
    }

    fn offer(
        &mut self,
        version: Version,
        page: String,
        problem: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let page = SharedString::from(page);
        self.checked_at = Some(SystemTime::now());
        self.status = UpdateStatus::Available {
            version: version.clone(),
            page: page.clone(),
        };
        let manual = std::mem::take(&mut self.manual);
        if manual || self.announced.as_ref() != Some(&version) {
            self.announced = Some(version.clone());
            cx.emit(UpdateEvent::Available {
                version,
                page,
                problem,
            });
        }
        cx.notify();
    }

    pub(crate) fn installed(&mut self, version: Version, cx: &mut Context<Self>) {
        log::info!("installed mtc {version}; it starts on the next launch");
        self.checked_at = Some(SystemTime::now());
        self.status = UpdateStatus::Ready {
            version: version.clone(),
        };
        self.manual = false;
        self.announced = Some(version.clone());
        cx.emit(UpdateEvent::Ready { version });
        cx.notify();
        // The menus read this updater's status, which can't be read while
        // it is being updated, so rebuild them once this update finishes.
        cx.defer(crate::actions::set_menus);
    }

    fn failed(&mut self, previous: UpdateStatus, err: anyhow::Error, cx: &mut Context<Self>) {
        log::warn!("checking for updates: {err:#}");
        self.status = previous;
        // Settings has room for the gist; the log has the details.
        self.error = Some(as_sentence(err.to_string()).into());
        if std::mem::take(&mut self.manual) {
            cx.emit(UpdateEvent::Failed {
                message: sentence(&err).into(),
            });
        }
        cx.notify();
    }
}

/// An error as a sentence: capitalized, with a final period.
fn sentence(err: &anyhow::Error) -> String {
    as_sentence(format!("{err:#}"))
}

fn as_sentence(text: String) -> String {
    let mut chars = text.chars();
    let mut sentence: String = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !sentence.ends_with(['.', '!', '?']) {
        sentence.push('.');
    }
    sentence
}

/// Tell the person in `window` about an update.
pub fn notify(event: &UpdateEvent, window: &mut Window, cx: &mut App) {
    let note = match event {
        UpdateEvent::Ready { version } => Notification::new()
            .title(format!("mtc {version} is ready"))
            .message("Restart to start using it, or it starts the next time you open mtc.")
            .action(|_, _, _| {
                Button::new("restart-to-update")
                    .primary()
                    .small()
                    .label("Restart")
                    .on_click(|_, _, cx| restart(cx))
            }),
        UpdateEvent::Available {
            version,
            page,
            problem,
        } => {
            let page = page.clone();
            let note = match problem {
                None => Notification::new()
                    .title(format!("mtc {version} is available"))
                    .message("Download it from the release page."),
                Some(problem) => Notification::warning(format!(
                    "{problem} Download it from the release page instead."
                ))
                .title(format!("Couldn’t install mtc {version}")),
            };
            note.action(move |_, _, _| {
                let page = page.clone();
                Button::new("download-update")
                    .small()
                    .label("Download")
                    .on_click(move |_, _, cx| cx.open_url(&page))
            })
        }
        UpdateEvent::UpToDate => {
            Notification::success(format!("mtc {} is the latest version.", current_version()))
        }
        UpdateEvent::Failed { message } => {
            Notification::error(format!("Couldn’t check for updates. {message}"))
        }
    };
    // A newer note about updates replaces the last one.
    window.remove_notification::<Updater>(cx);
    window.push_notification(note.id::<Updater>(), cx);
}

/// A title bar button for an update waiting on the person, if there is one.
pub fn title_bar_button(cx: &App) -> Option<AnyElement> {
    match status(cx) {
        UpdateStatus::Ready { version } => Some(
            Button::new("title-restart-to-update")
                .primary()
                .xsmall()
                .label("Restart to update")
                .tooltip(format!("mtc {version} is installed"))
                .on_click(|_, _, cx| restart(cx))
                .into_any_element(),
        ),
        UpdateStatus::Available { version, page } => Some(
            Button::new("title-update-available")
                .outline()
                .xsmall()
                .label("Update available")
                .tooltip(format!("Download mtc {version}"))
                .on_click(move |_, _, cx| cx.open_url(&page))
                .into_any_element(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::TestAppContext;

    use super::*;

    #[gpui_kit::test]
    fn installing_an_update_rebuilds_the_menus_without_panicking(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init(cx);
            crate::actions::init(cx);
        });
        let updater = cx.update(|cx| updater(cx));
        updater.update(cx, |updater, cx| {
            updater.installed(Version::new(9, 0, 0), cx);
        });
        cx.run_until_parked();
        assert_eq!(
            cx.update(|cx| status(cx)),
            UpdateStatus::Ready {
                version: Version::new(9, 0, 0)
            }
        );
    }
}
