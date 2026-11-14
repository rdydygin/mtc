//! Single sign-on in the system browser.
//!
//! [`SsoPanel`] shows the server's SSO providers. Choosing one opens the
//! browser; the server's last page hands the result back through a
//! `mattermost-dev://` URL (server 9.1 and later) or an `mmauth://` URL
//! (older servers), which reaches [`handle_open_urls`]: from macOS when mtc
//! runs as a bundle (see `script/bundle-macos`), or through `deep_link` on
//! Linux and Windows. If the browser can't hand it back, the user pastes the
//! link instead.

use std::time::{Duration, Instant};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, StyledExt as _, ThemeStyled as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    separator::Separator,
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window,
    base::TestSupportExt as _, div, prelude::FluentBuilder as _,
};
use mattermost::{
    Client, User,
    sso::{self, Handoff, LoginOptions, SsoAttempt, SsoKind, SsoProvider},
};

/// Wait this long after the last keystroke before asking a server how to
/// sign in.
const LOOKUP_DELAY: Duration = Duration::from_millis(450);

/// A callback that arrives with no panel on screen is shown by the next panel
/// opened within this long (the system can launch mtc with the URL).
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
    window: AnyWindowHandle,
    confirmation_focus: FocusHandle,
    provider_focus: FocusHandle,
    // The validated legacy session stays local until the user accepts it.
    confirmation: Option<(Client, User)>,
    revision: u64,
    server: String,
    lookup: Lookup,
    pending: Option<(SsoAttempt, SharedString)>,
    completing: bool,
    error: Option<SharedString>,
    paste: Entity<InputState>,
    _lookup_task: Option<Task<()>>,
    _completion_task: Option<Task<()>>,
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
            window: window.window_handle(),
            confirmation_focus: cx.focus_handle(),
            provider_focus: cx.focus_handle().tab_stop(true),
            confirmation: None,
            revision: 0,
            server: String::new(),
            lookup: Lookup::Idle,
            pending: None,
            completing: false,
            error: orphaned.then(|| ORPHANED.into()),
            paste,
            _lookup_task: None,
            _completion_task: None,
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
        self.cancel_completion();
        self.error = None;
        if server.is_empty() {
            self.lookup = Lookup::Idle;
            self._lookup_task = None;
            cx.notify();
            return;
        }
        self.look_up(LOOKUP_DELAY, cx);
        cx.notify();
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
        self.cancel_completion();
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

    /// A callback URL from the browser, delivered by the system.
    fn receive(&mut self, url: String, cx: &mut Context<Self>) {
        if self.pending.is_some() {
            self.complete(url, cx);
        } else if !self.completing {
            self.error = Some(ORPHANED.into());
            cx.notify();
        }
    }

    /// Dropping the task prevents a late result from signing in after cancellation.
    fn cancel_completion(&mut self) {
        self.revision += 1;
        self.confirmation = None;
        self._completion_task = None;
        self.completing = false;
        self.pending = None;
    }

    fn cancel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel_completion();
        self.error = None;
        self.paste.update(cx, |p, cx| p.set_value("", window, cx));
        self.provider_focus.focus(window, cx);
        cx.notify();
    }

    fn use_different_account(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((client, _)) = self.confirmation.take() {
            // Revocation must survive leaving this panel, but never block retrying.
            cx.background_spawn(async move {
                let _ = client.logout().await;
            })
            .detach();
        }
        self.cancel(window, cx);
    }

    fn confirm(&mut self, revision: u64, cx: &mut Context<Self>) {
        if revision != self.revision {
            return;
        }
        if let Some((client, me)) = self.confirmation.take() {
            self.cancel_completion();
            cx.emit(SsoEvent::SignedIn { client, me });
            cx.notify();
        }
    }

    fn complete(&mut self, callback: String, cx: &mut Context<Self>) {
        let Some((sso, _)) = self.pending.clone() else {
            return;
        };
        if callback.trim().is_empty() || (self.completing && sso.handoff() == Handoff::Desktop) {
            return;
        }
        self.finish_sign_in(async move { sso.complete(&callback).await }, cx);
    }

    fn finish_sign_in(
        &mut self,
        completion: impl Future<Output = anyhow::Result<(Client, User)>> + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some((attempt, _)) = &self.pending else {
            return;
        };
        let handoff = attempt.handoff();
        // A newer callback replaces both the validation task and any decision.
        self.revision += 1;
        let revision = self.revision;
        self.confirmation = None;
        self.completing = true;
        self.error = None;
        cx.notify();
        self._completion_task = Some(cx.spawn(async move |this, cx| {
            let result = completion.await;
            let _ = this.update(cx, |this, cx| {
                if this.revision != revision {
                    return;
                }
                this.completing = false;
                match result {
                    Ok((client, me)) if handoff == Handoff::Mobile => {
                        this.show_confirmation(client, me, cx);
                    }
                    Ok((client, me)) => {
                        this.pending = None;
                        cx.emit(SsoEvent::SignedIn { client, me });
                    }
                    Err(err) => this.error = Some(format!("{err:#}").into()),
                }
                cx.notify();
            });
        }));
    }

    fn show_confirmation(&mut self, client: Client, me: User, cx: &mut Context<Self>) {
        self.confirmation = Some((client, me));
        // Focus the decision, never its commit button: Enter from the paste
        // field (or a held key) must not accept an account.
        let focus = self.confirmation_focus.clone();
        let paste = self.paste.clone();
        let _ = self.window.update(cx, |_, window, cx| {
            paste.update(cx, |paste, cx| paste.set_value("", window, cx));
            focus.focus(window, cx);
        });
    }

    fn render_confirmation(
        &self,
        client: &Client,
        me: &User,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let name = me.display_name();
        let title = format!("Continue as {name}?");
        let account = format!("@{}", me.username);
        // Include the port and site path as well as the host for self-hosted servers.
        let server = client.server().as_str().trim_end_matches('/').to_string();
        let revision = self.revision;
        v_flex()
            // Pressed state belongs to this decision, not the panel location.
            // Replacing an account must discard native mouse/key-down state.
            .id(("sso-confirmation", revision))
            .key_context("SsoConfirmation")
            .on_action(
                cx.listener(|this, _: &crate::actions::Dismiss, window, cx| {
                    this.use_different_account(window, cx);
                }),
            )
            .gap_4()
            .child(
                v_flex()
                    .id("sso-account")
                    .role(gpui_kit::Role::Group)
                    .aria_label(format!("{title} {account} on {server}"))
                    .test_support()
                    .track_focus(&self.confirmation_focus)
                    .when(self.confirmation_focus.is_focused(window), |this| {
                        this.focus_ring_style(window, cx)
                    })
                    .gap_1()
                    .child(div().text_lg().font_semibold().child(title))
                    .child(div().text_sm().child(account))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(server),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .child("Check that this is the account you want to use."),
            )
            .child(
                v_flex()
                    .gap_2()
                    // Keep visual and Tab order aligned. A queued Tab followed
                    // by Enter/Space must reach the safe action on arrival.
                    .child(
                        Button::new("different-sso-account")
                            .outline()
                            .w_full()
                            .label("Use a different account")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.use_different_account(window, cx)
                            })),
                    )
                    .child(
                        Button::new("confirm-sso")
                            .primary()
                            .w_full()
                            .label(format!("Continue as @{}", me.username))
                            .on_click(
                                cx.listener(move |this, _, _, cx| this.confirm(revision, cx)),
                            ),
                    ),
            )
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
            .child(
                Input::new(&self.paste)
                    .id("sso-paste")
                    .disabled(self.completing),
            )
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let error = self.error.clone();
        let content = if let Some((client, me)) = &self.confirmation {
            Some(
                self.render_confirmation(client, me, window, cx)
                    .into_any_element(),
            )
        } else if let Some((sso, provider)) = self.pending.clone() {
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
                            .children(providers.into_iter().enumerate().map(|(ix, provider)| {
                                let label = format!("Continue with {}", provider.label());
                                let id = SharedString::from(format!("sso-{:?}", provider.kind()));
                                Button::new(id)
                                    .when(ix == 0, |button| {
                                        button.track_focus(&self.provider_focus)
                                    })
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
                        // Wrap the reason instead of pushing the button out
                        // of the panel: a flex item won't shrink below its
                        // text's width otherwise.
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "Couldn’t check this server’s sign-in options. {reason}"
                                )),
                        )
                        .child(
                            Button::new("retry-sso-lookup")
                                .flex_shrink_0()
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

/// Ask the system to send our callback schemes to this app. On macOS this
/// only works when mtc runs from an app bundle; a bare `cargo run` binary
/// falls back to pasting. Linux and Windows register the executable itself.
#[cfg(not(target_os = "macos"))]
pub fn register_callback_schemes(_: &mut App) {
    crate::deep_link::register();
}

/// Ask the system to send our callback schemes to this app. On macOS this
/// only works when mtc runs from an app bundle; a bare `cargo run` binary
/// falls back to pasting. Linux and Windows register the executable itself.
#[cfg(target_os = "macos")]
pub fn register_callback_schemes(cx: &mut App) {
    use mattermost::sso::{CALLBACK_SCHEME, MOBILE_CALLBACK_SCHEME};
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

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Bounds, TestAppContext, WindowBounds, WindowOptions, point, px, size};

    use super::*;

    fn signed_in() -> anyhow::Result<(Client, User)> {
        Ok((
            Client::new_unverified("https://chat.example.com", "token")?,
            User::default(),
        ))
    }

    fn begin_completion(
        panel: &Entity<SsoPanel>,
        cx: &mut App,
    ) -> futures::channel::oneshot::Sender<anyhow::Result<(Client, User)>> {
        let (send, receive) = futures::channel::oneshot::channel();
        panel.update(cx, |panel, cx| {
            panel.pending = Some((
                SsoAttempt::new("chat.example.com", Handoff::Desktop).unwrap(),
                "SSO".into(),
            ));
            panel.finish_sign_in(async move { receive.await.unwrap() }, cx);
        });
        send
    }

    #[gpui_kit::test]
    fn cancel_and_server_changes_discard_in_flight_sign_in(cx: &mut TestAppContext) {
        use std::{cell::Cell, rc::Rc};
        cx.update(gpui_kit::init);
        let (window, panel) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| SsoPanel::new(window, cx))
            })
            .unwrap()
        });
        let events = Rc::new(Cell::new(0));
        let count = events.clone();
        let _subscription =
            cx.update(|cx| cx.subscribe(&panel, move |_, _, _| count.set(count.get() + 1)));
        let cancelled = cx.update(|cx| begin_completion(&panel, cx));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("cancel-sso", cx);
        })
        .unwrap();
        // Complete before polling the cancelled task again, as a racing server might.
        let _ = cancelled.send(signed_in());
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(!panel.read(cx).completing);
            assert!(!panel.read(cx).is_waiting());
            assert!(panel.read(cx).error.is_none());
        });
        assert_eq!(events.get(), 0);

        let stale = cx.update(|cx| begin_completion(&panel, cx));
        cx.run_until_parked();
        cx.update(|cx| panel.update(cx, |panel, cx| panel.set_server("other.example.com", cx)));
        let _ = stale.send(signed_in());
        cx.run_until_parked();
        assert_eq!(events.get(), 0);
        cx.update(|cx| assert!(!panel.read(cx).completing));

        let current = cx.update(|cx| begin_completion(&panel, cx));
        current.send(signed_in()).ok().unwrap();
        cx.run_until_parked();
        assert_eq!(events.get(), 1);
        cx.update(|cx| {
            assert!(!panel.read(cx).is_waiting());
            assert!(!panel.read(cx).completing);
            assert!(panel.read(cx).confirmation.is_none());
        });
    }

    fn legacy_server() -> mattermost::testing::SlowServer {
        use mattermost::testing::{Network, Reply, SlowServer};
        use serde_json::json;
        SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (request.method(), request.path()) {
                ("GET", "/api/v4/config/client?format=old") => Reply::json(json!({
                    "Version": "9.0.0", "EnableSignUpWithGitLab": "true"
                })),
                ("GET", "/api/v4/users/me") => {
                    let username = if request.header("Authorization") == Some("Bearer bob") {
                        "bob"
                    } else {
                        "ann"
                    };
                    Reply::json(json!({"id": username, "username": username,
                        "first_name": if username == "ann" { "Ann" } else { "Bob" },
                        "last_name": "Lee"}))
                }
                // Even failed revocation must leave the user free to retry.
                ("POST", "/api/v4/users/logout") => Reply::json(json!({})).status(503),
                _ => Reply::not_found(),
            }
        })
    }

    fn legacy_panel(
        server: &mattermost::testing::SlowServer,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<SsoPanel>) {
        cx.executor().allow_parking();
        let options = futures::executor::block_on(sso::login_options(&server.url())).unwrap();
        cx.update(gpui_kit::init);
        cx.update(crate::actions::init);
        let (window, panel) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| {
                    let mut panel = SsoPanel::new(window, cx);
                    panel.server = server.url();
                    panel.lookup = Lookup::Loaded(options);
                    panel
                })
            })
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("sso-GitLab", cx);
        })
        .unwrap();
        (window, panel)
    }

    fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for SSO");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_confirmation(panel: &Entity<SsoPanel>, cx: &mut TestAppContext) {
        wait_until(cx, |cx| {
            panel.read_with(cx, |p, _| p.confirmation.is_some())
        });
    }

    #[gpui_kit::test]
    fn legacy_links_and_paste_require_explicit_account_confirmation(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};
        let server = legacy_server();
        let (window, panel) = legacy_panel(&server, cx);
        let accounts = Rc::new(RefCell::new(Vec::new()));
        let observed = accounts.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&panel, move |_, event, _| {
                let SsoEvent::SignedIn { client, me } = event;
                observed
                    .borrow_mut()
                    .push((me.username.clone(), client.server().to_string()));
            })
        });
        for pasted in [false, true] {
            if pasted {
                cx.update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    window.click("sso-GitLab", cx);
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                        "mmauth://callback?MMAUTHTOKEN=ann".into(),
                    ));
                    window.click("sso-paste", cx);
                    window.press("secondary-v", cx);
                })
                .unwrap();
            } else {
                cx.update(|cx| {
                    handle_open_urls(vec!["mmauth://callback?MMAUTHTOKEN=ann".into()], cx)
                });
            }
            wait_for_confirmation(&panel, cx);
            assert_eq!(accounts.borrow().len(), usize::from(pasted));
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                assert_eq!(
                    window.find("sso-account").label(),
                    Some(format!("Continue as Ann Lee? @ann on {}", server.url()).as_str())
                );
                assert!(panel.read(cx).confirmation_focus.is_focused(window));
                window.press("enter", cx);
                assert!(
                    panel.read(cx).confirmation.is_some(),
                    "stray Enter accepted account"
                );
                if pasted {
                    window.click("confirm-sso", cx);
                } else {
                    window.press("tab", cx);
                    assert_eq!(window.find("different-sso-account").focused(), Some(true));
                    window.press("tab", cx);
                    assert_eq!(window.find("confirm-sso").focused(), Some(true));
                    window.press("enter", cx);
                }
            })
            .unwrap();
            cx.run_until_parked();
            assert_eq!(accounts.borrow().len(), 1 + usize::from(pasted));
            assert_eq!(
                accounts.borrow().last().unwrap(),
                &("ann".into(), format!("{}/", server.url()))
            );
        }
    }

    #[gpui_kit::test]
    fn different_account_and_escape_discard_and_revoke_without_waiting(cx: &mut TestAppContext) {
        let server = legacy_server();
        let (window, panel) = legacy_panel(&server, cx);
        let _subscription =
            cx.update(|cx| cx.subscribe(&panel, |_, _, _| panic!("discard signed in")));
        for escape in [false, true] {
            cx.update(|cx| {
                panel.update(cx, |p, cx| {
                    p.receive("mmauth://callback?MMAUTHTOKEN=ann".into(), cx)
                })
            });
            wait_for_confirmation(&panel, cx);
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                if escape {
                    window.press("tab", cx);
                    window.press("escape", cx);
                } else {
                    window.click("different-sso-account", cx);
                }
                assert!(panel.read(cx).confirmation.is_none());
                assert!(!panel.read(cx).is_waiting());
                assert!(window.find("sso-GitLab").visible());
                assert_eq!(window.find("sso-GitLab").focused(), Some(true));
                assert!(panel.read(cx).paste.read(cx).value().is_empty());
            })
            .unwrap();
            wait_until(cx, |_| {
                server
                    .requests()
                    .iter()
                    .filter(|r| r.method() == "POST" && r.path() == "/api/v4/users/logout")
                    .count()
                    == 1 + usize::from(escape)
            });
            if !escape {
                cx.update_window(window, |_, window, cx| window.click("sso-GitLab", cx))
                    .unwrap();
            }
        }
    }

    #[gpui_kit::test]
    fn newer_callbacks_replace_the_account_and_reject_old_actions(cx: &mut TestAppContext) {
        let server = legacy_server();
        let (window, panel) = legacy_panel(&server, cx);
        let _subscription =
            cx.update(|cx| cx.subscribe(&panel, |_, _, _| panic!("stale action signed in")));
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                p.receive("mmauth://callback?MMAUTHTOKEN=ann".into(), cx)
            })
        });
        wait_for_confirmation(&panel, cx);
        let old_revision = panel.read_with(cx, |p, _| p.revision);
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                p.receive("mmauth://callback?MMAUTHTOKEN=bob".into(), cx);
                assert!(p.confirmation.is_none());
            })
        });
        wait_for_confirmation(&panel, cx);
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                assert_eq!(p.confirmation.as_ref().unwrap().1.username, "bob");
                p.confirm(old_revision, cx);
                assert!(p.confirmation.is_some());
            })
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("confirm-sso").label(), Some("Continue as @bob"));
        })
        .unwrap();
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                p.receive("mmauth://callback?invalid".into(), cx)
            })
        });
        wait_until(cx, |cx| panel.read_with(cx, |p, _| !p.completing));
        cx.update(|cx| {
            assert!(panel.read(cx).confirmation.is_none());
            assert!(panel.read(cx).error.is_some());
        });
    }

    /// Resolve validation through the production completion task without drawing
    /// a waiting frame between the old and new accounts.
    fn resolve_account(panel: &Entity<SsoPanel>, username: &str, cx: &mut TestAppContext) {
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                let client = Client::new_unverified(&p.server, username).unwrap();
                let mut me = User::default();
                me.username = username.into();
                me.first_name = username.into();
                p.finish_sign_in(async move { Ok((client, me)) }, cx);
            })
        });
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn replacing_confirmation_cancels_a_mouse_press_on_the_previous_account(
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{MouseButton, test::TestEventExt as _};
        use std::{cell::RefCell, rc::Rc};
        let server = legacy_server();
        let (window, panel) = legacy_panel(&server, cx);
        let accounts = Rc::new(RefCell::new(Vec::new()));
        let observed = accounts.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&panel, move |_, event, _| {
                let SsoEvent::SignedIn { me, .. } = event;
                observed.borrow_mut().push(me.username.clone());
            })
        });
        resolve_account(&panel, "ann", cx);
        let down = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                let point = window.find("confirm-sso").bounds().center();
                window.pointer_move(point, None, cx);
                window.pointer_down(point, MouseButton::Left, cx);
                point
            })
            .unwrap();
        // No render while validation replaces Ann with Bob. The next frame
        // must not inherit Ann's pending mouse-down at the same coordinates.
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                p.finish_sign_in(std::future::pending(), cx);
                // Deliver the validated result synchronously: running the executor
                // here could draw the waiting state and accidentally mask the race.
                p.completing = false;
                let client = Client::new_unverified(&server.url(), "bob").unwrap();
                let mut me = User::default();
                me.username = "bob".into();
                me.first_name = "bob".into();
                p.show_confirmation(client, me, cx);
            })
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("confirm-sso").label(), Some("Continue as @bob"));
            assert_eq!(window.find("confirm-sso").bounds().center(), down);
            window.pointer_up(down, MouseButton::Left, cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert!(
            accounts.borrow().is_empty(),
            "mouse-up accepted a replacement account"
        );
        cx.update_window(window, |_, window, cx| window.click("confirm-sso", cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(&*accounts.borrow(), &["bob"]);
    }

    #[gpui_kit::test]
    fn queued_login_keys_cannot_accept_an_arriving_account(cx: &mut TestAppContext) {
        use crate::login::LoginView;
        use gpui_kit::test::TestEventExt as _;
        let server = legacy_server();
        cx.executor().allow_parking();
        let options = futures::executor::block_on(sso::login_options(&server.url())).unwrap();
        cx.update(gpui_kit::init);
        cx.update(crate::actions::init);
        let (window, login) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                cx.new(|cx| LoginView::new(None, window, cx))
            })
            .unwrap()
        });
        let panel = cx.update(|cx| {
            cx.global::<SsoCallbacks>()
                .panel
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap()
        });
        let _subscription =
            cx.update(|cx| cx.subscribe(&login, |_, _, _| panic!("queued keys signed in")));
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window.click("login-server", cx);
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(server.url()));
            window.press("secondary-a", cx);
            window.press("secondary-v", cx);
        })
        .unwrap();
        cx.run_until_parked();
        // Keep provider discovery deterministic; exercise the real LoginView,
        // server field, SSO task, focus tree and SignedIn forwarding below.
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                p._lookup_task = None;
                p.server = server.url();
                p.lookup = Lookup::Loaded(options);
                cx.notify();
            })
        });
        for key in ["enter", "space"] {
            for held in [false, true] {
                cx.update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    window.click("sso-GitLab", cx);
                    window.click("login-server", cx);
                    assert_eq!(window.find("login-server").focused(), Some(true));
                    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(
                        "mmauth://callback?MMAUTHTOKEN=ann".into(),
                    ));
                })
                .unwrap();
                // The injected callback resolves before queued Tab/paste/Enter
                // (or Space/repeat) intended for the waiting paste input runs.
                resolve_account(&panel, "bob", cx);
                cx.update_window(window, |_, window, cx| {
                    window.render_frame(cx);
                    assert!(panel.read(cx).confirmation_focus.is_focused(window));
                    // Unaccompanied activation and repeat on arrival are inert.
                    window.key_down(key, held, cx);
                    window.key_up(key, cx);
                    assert!(panel.read(cx).confirmation.is_some());
                    window.press("tab", cx);
                    assert_eq!(window.find("different-sso-account").focused(), Some(true));
                    window.press("secondary-v", cx);
                    window.key_down(key, false, cx);
                    if held {
                        window.key_down(key, true, cx);
                        window.key_down(key, true, cx);
                    }
                    window.key_up(key, cx);
                })
                .unwrap();
                cx.run_until_parked();
                assert!(!panel.read_with(cx, |p, _| p.is_waiting()));
                assert!(panel.read_with(cx, |p, _| p.confirmation.is_none()));
            }
        }
    }

    #[gpui_kit::test]
    fn cancelling_changing_server_and_restarting_clear_confirmations(cx: &mut TestAppContext) {
        let server = legacy_server();
        let (window, panel) = legacy_panel(&server, cx);
        let _subscription =
            cx.update(|cx| cx.subscribe(&panel, |_, _, _| panic!("cancelled account signed in")));
        for change in ["cancel", "server", "start", "new callback"] {
            let (send, receive) = futures::channel::oneshot::channel();
            cx.update(|cx| {
                panel.update(cx, |p, cx| {
                    p.pending = Some((
                        SsoAttempt::new(&server.url(), Handoff::Mobile).unwrap(),
                        "GitLab".into(),
                    ));
                    p.finish_sign_in(async move { receive.await.unwrap() }, cx);
                })
            });
            send.send(signed_in()).ok().unwrap();
            cx.run_until_parked();
            assert!(panel.read_with(cx, |p, _| p.confirmation.is_some()));
            let old_revision = panel.read_with(cx, |p, _| p.revision);
            cx.update_window(window, |_, window, cx| {
                panel.update(cx, |p, cx| {
                    match change {
                        "cancel" => p.cancel(window, cx),
                        "server" => p.set_server("", cx),
                        "start" => {
                            p.server = server.url();
                            p.lookup = Lookup::Loaded(
                                futures::executor::block_on(sso::login_options(&server.url()))
                                    .unwrap(),
                            );
                            let Lookup::Loaded(options) = &p.lookup else {
                                unreachable!()
                            };
                            p.start(&options.providers[0].clone(), cx);
                        }
                        _ => p.finish_sign_in(std::future::pending(), cx),
                    }
                    assert!(p.confirmation.is_none());
                    p.confirm(old_revision, cx);
                })
            })
            .unwrap();
        }
        // Replace a validation still in flight; even a racing success is obsolete.
        let (send, receive) = futures::channel::oneshot::channel();
        cx.update(|cx| {
            panel.update(cx, |p, cx| {
                p.finish_sign_in(async move { receive.await.unwrap() }, cx);
            })
        });
        cx.run_until_parked();
        cx.update(|cx| panel.update(cx, |p, cx| p.finish_sign_in(std::future::pending(), cx)));
        let _ = send.send(signed_in());
        cx.run_until_parked();
        assert!(panel.read_with(cx, |p, _| p.confirmation.is_none()));
    }

    /// The panel in a column as narrow as the sign-in card's.
    struct Narrow(Entity<SsoPanel>);

    const WIDTH: f32 = 360.;

    impl Render for Narrow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex().w(px(WIDTH)).child(self.0.clone())
        }
    }

    #[gpui_kit::test]
    fn account_confirmation_fits_the_sign_in_column_at_larger_text_sizes(cx: &mut TestAppContext) {
        use gpui_kit::component::{Theme, ThemeMode};
        cx.update(gpui_kit::init);
        let (window, _) = cx.update(|cx| {
            gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
                let panel = cx.new(|cx| {
                    let mut panel = SsoPanel::new(window, cx);
                    let mut me = User::default();
                    me.first_name = "Ann Alexandra Catherine".into();
                    me.last_name = "Lee de la Cruz".into();
                    me.username = "ann.alexandra.lee".into();
                    panel.confirmation = Some((
                        Client::new_unverified("https://chat.example.com/mattermost", "token")
                            .unwrap(),
                        me,
                    ));
                    panel.confirmation_focus.focus(window, cx);
                    panel
                });
                cx.new(|_| Narrow(panel))
            })
            .unwrap()
        });
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            for font_size in [16., 20.] {
                cx.update_window(window, |_, window, cx| {
                    Theme::change(mode, Some(window), cx);
                    Theme::update(cx, |theme| theme.font_size = px(font_size));
                    window.render_frame(cx);
                    let account = window.find("sso-account").bounds();
                    let confirm = window.find("confirm-sso").bounds();
                    let different = window.find("different-sso-account").bounds();
                    assert_eq!(account.left(), confirm.left());
                    assert_eq!(confirm.left(), different.left());
                    assert_eq!(confirm.right(), different.right());
                    assert!(account.right() <= px(WIDTH));
                    assert!(confirm.right() <= px(WIDTH));
                    assert!(different.top() > account.bottom());
                    assert!(confirm.top() > different.bottom());
                })
                .unwrap();
            }
        }
    }

    #[gpui_kit::test]
    fn a_long_lookup_error_wraps_inside_the_panel(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let window = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: point(px(0.), px(0.)),
                        size: size(px(1000.), px(400.)),
                    })),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let panel = cx.new(|cx| {
                        let mut panel = SsoPanel::new(window, cx);
                        panel.lookup = Lookup::Failed(
                            "Couldn’t reach the server. Check the address and your connection."
                                .into(),
                        );
                        panel
                    });
                    cx.new(|_| Narrow(panel))
                },
            )
            .unwrap()
            .0
        });
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let retry = window.find("retry-sso-lookup").bounds();
            assert!(retry.size.width > px(0.));
            assert!(
                retry.right() <= px(WIDTH),
                "Try again ends at {:?}, past the {WIDTH}px panel",
                retry.right()
            );
        })
        .unwrap();
    }
}
