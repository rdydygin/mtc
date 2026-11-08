//! Single sign-on in the system browser.
//!
//! [`SsoPanel`] shows the server's SSO providers. Choosing one opens the
//! browser; the server's last page hands the result back through a
//! `mattermost-dev://` URL (server 9.1 and later) or an `mmauth://` URL
//! (older servers), which macOS delivers to [`handle_open_urls`] when mtc
//! runs as a bundle (see `script/bundle-macos`). If the browser can't hand it
//! back, the user pastes the link instead.

use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    separator::Separator,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, Global, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use mattermost::{
    Client, User,
    sso::{
        self, CALLBACK_SCHEME, Handoff, LoginOptions, MOBILE_CALLBACK_SCHEME, SsoAttempt, SsoKind,
        SsoProvider,
    },
};

/// Wait this long after the last keystroke before asking a server how to
/// sign in.
const LOOKUP_DELAY: Duration = Duration::from_millis(450);

/// A callback that arrives with no panel on screen is shown by the next panel
/// opened within this long (macOS can launch mtc with the URL).
const ORPHAN_WINDOW: Duration = Duration::from_secs(120);

const ORPHANED: &str = "That sign-in was cancelled or mtc restarted before it finished. \
                        Choose a sign-in method to start again.";

pub enum SsoEvent {
    SignedIn { client: Client, me: User },
}

enum Lookup {
    Idle,
    Loading,
    Loaded(LoginOptions),
    Failed(SharedString),
}

/// Where browser callbacks go: the sign-in panel on screen, or, when none
/// is, the time one arrived, for the next panel to explain.
#[derive(Default)]
struct SsoCallbacks {
    panel: Option<WeakEntity<SsoPanel>>,
    orphaned_at: Option<Instant>,
}

impl Global for SsoCallbacks {}

pub struct SsoPanel {
    server: String,
    lookup: Lookup,
    pending: Option<(SsoAttempt, SharedString)>,
    completing: bool,
    error: Option<SharedString>,
    paste: Entity<InputState>,
    _lookup_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SsoEvent> for SsoPanel {}

impl SsoPanel {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let paste = cx.new(|cx| InputState::new(window, cx).placeholder("Paste the sign-in link"));
        let subscription = cx.subscribe_in(&paste, window, |this, paste, event, _, cx| {
            if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                let text = paste.read(cx).value().to_string();
                let ours = this
                    .pending
                    .as_ref()
                    .is_some_and(|(sso, _)| sso.matches(&text));
                if ours || matches!(event, InputEvent::PressEnter { .. }) {
                    this.complete(text, cx);
                }
            }
        });
        let this = cx.weak_entity();
        let callbacks = cx.default_global::<SsoCallbacks>();
        callbacks.panel = Some(this);
        let orphaned = callbacks
            .orphaned_at
            .take()
            .is_some_and(|at| at.elapsed() < ORPHAN_WINDOW);
        Self {
            server: String::new(),
            lookup: Lookup::Idle,
            pending: None,
            completing: false,
            error: orphaned.then(|| ORPHANED.into()),
            paste,
            _lookup_task: None,
            _subscriptions: vec![subscription],
        }
    }

    /// Follow the server address field; looks up providers after a pause.
    pub fn set_server(&mut self, server: &str, cx: &mut Context<Self>) {
        let server = server.trim().to_string();
        if server == self.server {
            return;
        }
        self.server = server.clone();
        self.pending = None;
        self.error = None;
        if server.is_empty() {
            self.lookup = Lookup::Idle;
            self._lookup_task = None;
            cx.notify();
            return;
        }
        self.look_up(LOOKUP_DELAY, cx);
    }

    fn look_up(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let server = self.server.clone();
        self._lookup_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                this.lookup = Lookup::Loading;
                cx.notify();
            });
            let result = sso::login_options(&server).await;
            let _ = this.update(cx, |this, cx| {
                if this.server != server {
                    return;
                }
                this.lookup = match result {
                    Ok(options) => Lookup::Loaded(options),
                    Err(err) => Lookup::Failed(format!("{err:#}").into()),
                };
                cx.notify();
            });
        }));
    }

    /// Whether the server accepts a password. Unknown until looked up, so
    /// the password form stays visible while loading or offline.
    pub fn allows_password(&self) -> bool {
        match &self.lookup {
            Lookup::Loaded(options) => options.password || options.providers.is_empty(),
            _ => true,
        }
    }

    pub fn is_waiting(&self) -> bool {
        self.pending.is_some()
    }

    fn start(&mut self, provider: &SsoProvider, cx: &mut Context<Self>) {
        let handoff = match &self.lookup {
            Lookup::Loaded(options) => options.handoff,
            _ => Handoff::default(),
        };
        let sso = match SsoAttempt::new(&self.server, handoff) {
            Ok(sso) => sso,
            Err(err) => {
                self.error = Some(format!("{err:#}").into());
                cx.notify();
                return;
            }
        };
        match sso.login_url(provider) {
            Ok(url) => cx.open_url(url.as_str()),
            Err(err) => {
                self.error = Some(format!("{err:#}").into());
                cx.notify();
                return;
            }
        }
        self.pending = Some((sso, provider.label().to_string().into()));
        self.error = None;
        cx.notify();
    }

    /// A callback URL from the browser, delivered by macOS.
    fn receive(&mut self, url: String, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            self.complete(url, cx);
        } else if !self.completing {
            self.error = Some(ORPHANED.into());
            cx.notify();
        }
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = None;
        self.error = None;
        self.paste.update(cx, |p, cx| p.set_value("", window, cx));
        cx.notify();
    }

    fn complete(&mut self, callback: String, cx: &mut Context<Self>) {
        let Some((sso, _)) = self.pending.clone() else {
            return;
        };
        if self.completing || callback.trim().is_empty() {
            return;
        }
        self.completing = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = sso.complete(&callback).await;
            let _ = this.update(cx, |this, cx| {
                this.completing = false;
                match result {
                    Ok((client, me)) => {
                        this.pending = None;
                        cx.emit(SsoEvent::SignedIn { client, me });
                    }
                    Err(err) => this.error = Some(format!("{err:#}").into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn provider_icon(kind: SsoKind) -> IconName {
        match kind {
            SsoKind::GitLab => IconName::GitFork,
            SsoKind::Google | SsoKind::Office365 => IconName::Globe,
            SsoKind::OpenId | SsoKind::Saml => IconName::KeyRound,
        }
    }

    fn render_waiting(
        &self,
        provider: &SharedString,
        handoff: Handoff,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let hint = match handoff {
            Handoff::Desktop => {
                "If the browser doesn’t bring you back, copy the address of the page \
                 it ends on and paste it here."
            }
            Handoff::Mobile => {
                "If the browser doesn’t bring you back, right-click the “click here” link \
                 on the page it ends on, copy the link, and paste it here."
            }
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_2()
                    .child(Spinner::new().small())
                    .child(div().text_sm().child(format!(
                        "Finish signing in with {provider} in your browser."
                    ))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(hint),
            )
            .child(Input::new(&self.paste).disabled(self.completing))
            .child(
                h_flex().justify_end().child(
                    Button::new("cancel-sso")
                        .ghost()
                        .small()
                        .label("Cancel")
                        .on_click(cx.listener(|this, _, window, cx| this.cancel(window, cx))),
                ),
            )
    }
}

impl Render for SsoPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.error.clone();
        let content = if let Some((sso, provider)) = self.pending.clone() {
            Some(
                self.render_waiting(&provider, sso.handoff(), cx)
                    .into_any_element(),
            )
        } else {
            match &self.lookup {
                Lookup::Loaded(options) if !options.providers.is_empty() => {
                    let providers = options.providers.clone();
                    let show_divider = options.password;
                    Some(
                        v_flex()
                            .gap_2()
                            .children(providers.into_iter().map(|provider| {
                                let label = format!("Continue with {}", provider.label());
                                let id = SharedString::from(format!("sso-{:?}", provider.kind()));
                                Button::new(id)
                                    .outline()
                                    .w_full()
                                    .icon(Self::provider_icon(provider.kind()))
                                    .label(label)
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| {
                                            this.start(&provider, cx)
                                        }),
                                    )
                            }))
                            .when(show_divider, |this| {
                                this.child(div().pt_2().child(Separator::horizontal().label("or")))
                            })
                            .into_any_element(),
                    )
                }
                Lookup::Loading => Some(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(Spinner::new().small())
                        .child("Checking how this server signs in…")
                        .into_any_element(),
                ),
                Lookup::Failed(reason) => Some(
                    h_flex()
                        .gap_2()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "Couldn’t check this server’s sign-in options. {reason}"
                                )),
                        )
                        .child(
                            Button::new("retry-sso-lookup")
                                .ghost()
                                .small()
                                .label("Try again")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.lookup = Lookup::Loading;
                                    this.look_up(Duration::ZERO, cx);
                                    cx.notify();
                                })),
                        )
                        .into_any_element(),
                ),
                _ => None,
            }
        };
        v_flex()
            .gap_2()
            .children(content)
            .when_some(error, |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }
}

/// Route SSO callback URLs to the sign-in panel. With no panel on screen
/// (signed in, or still starting up), the next panel says to start again.
pub fn handle_open_urls(urls: Vec<String>, cx: &mut App) {
    let urls: Vec<_> = urls
        .into_iter()
        .filter(|url| sso::is_callback_url(url))
        .collect();
    if urls.is_empty() {
        return;
    }
    cx.activate(true);
    let callbacks = cx.default_global::<SsoCallbacks>();
    let Some(panel) = callbacks.panel.as_ref().and_then(WeakEntity::upgrade) else {
        callbacks.orphaned_at = Some(Instant::now());
        return;
    };
    for url in urls {
        panel.update(cx, |panel, cx| panel.receive(url, cx));
    }
}

/// Ask macOS to send our callback schemes to this app. Only works when mtc
/// runs from an app bundle; a bare `cargo run` binary falls back to pasting.
pub fn register_callback_schemes(cx: &mut App) {
    for scheme in [CALLBACK_SCHEME, MOBILE_CALLBACK_SCHEME] {
        let task = cx.register_url_scheme(scheme);
        cx.background_spawn(async move {
            if let Err(err) = task.await {
                log::info!(
                    "SSO callback scheme {scheme}:// not registered ({err:#}); \
                     paste-the-link fallback only"
                );
            }
        })
        .detach();
    }
}
