//! The window's top-level view: restores a saved session, shows sign-in, or
//! hosts the workspace.

use gpui_kit::component::{ActiveTheme as _, TitleBar, v_flex};
use gpui_kit::{
    AppContext as _, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};
use mattermost::{ApiError, Client, TeamId, User};

use crate::{
    actions::{ShowSettings, SignOut},
    appearance, cache,
    http::ServerHttpClient,
    login::{LoginEvent, LoginView},
    persist::{self, SavedSession},
    settings, skeleton, update,
    workspace::{Workspace, WorkspaceEvent},
};

enum Screen {
    Restoring,
    SignedOut(Entity<LoginView>),
    SignedIn(Entity<Workspace>),
}

pub struct AppRoot {
    screen: Screen,
    _screen_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
}

impl AppRoot {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe_window_appearance(window, |_, window, cx| {
                appearance::system_appearance_changed(window, cx);
            }),
            cx.subscribe_in(&update::updater(cx), window, |_, _, event, window, cx| {
                update::notify(event, window, cx);
            }),
            cx.observe(&update::updater(cx), |_, _, cx| cx.notify()),
            cx.observe_window_activation(window, |this, window, cx| {
                if let Screen::SignedIn(workspace) = &this.screen {
                    let active = window.is_window_active();
                    workspace.update(cx, |w, cx| w.set_window_active(active, cx));
                }
            }),
        ];
        let mut this = Self {
            screen: Screen::Restoring,
            _screen_subscription: None,
            _subscriptions: subscriptions,
        };
        match persist::load().session {
            Some(saved) => this.restore(saved, window, cx),
            None => this.show_login(None, window, cx),
        }
        this
    }

    fn restore(&mut self, saved: SavedSession, window: &mut Window, cx: &mut Context<Self>) {
        // With the profile saved last time, the window opens at once and
        // every startup request goes out together. The token is checked by
        // those same requests: if the server rejects it, the session signs
        // out and the sign-in screen says why.
        if let Some(me) = saved.user.clone()
            && let Ok(client) = Client::new_unverified(&saved.server, &saved.token)
        {
            let team = saved.team_id.map(TeamId::from);
            self.sign_in(client, me, team, window, cx);
            if let Screen::SignedIn(workspace) = &self.screen {
                let refresh = workspace.update(cx, |w, cx| w.refresh_me(cx));
                cx.spawn(async move |_, _| {
                    if let Ok(me) = refresh.await {
                        persist::update(|state| {
                            if let Some(session) = state.session.as_mut() {
                                session.user = Some((*me).clone());
                            }
                        });
                    }
                })
                .detach();
            }
            return;
        }
        // Sessions saved by older versions have no profile: wait for it.
        self.screen = Screen::Restoring;
        cx.spawn_in(window, async move |this, cx| {
            let result = Client::with_token(&saved.server, &saved.token).await;
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok((client, me)) => {
                    let team = saved.team_id.map(TeamId::from);
                    this.sign_in(client, me, team, window, cx);
                }
                Err(err) => {
                    let message = if err.downcast_ref::<ApiError>() == Some(&ApiError::Unauthorized)
                    {
                        end_session();
                        "Your session has expired. Sign in again.".to_string()
                    } else {
                        format!("{err:#}")
                    };
                    this.show_login(Some(message.into()), window, cx);
                }
            });
        })
        .detach();
    }

    fn show_login(
        &mut self,
        error: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let login = cx.new(|cx| LoginView::new(error, window, cx));
        self._screen_subscription =
            Some(
                cx.subscribe_in(&login, window, |this, _, event, window, cx| match event {
                    LoginEvent::SignedIn { client, me } => {
                        this.sign_in(client.clone(), me.clone(), None, window, cx)
                    }
                }),
            );
        login.update(cx, |login, cx| login.focus_first_field(window, cx));
        self.screen = Screen::SignedOut(login);
        cx.notify();
    }

    fn sign_in(
        &mut self,
        client: Client,
        me: User,
        team: Option<TeamId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Lets avatars and thumbnails load through GPUI's image cache.
        cx.set_http_client(ServerHttpClient::new(client.clone()));
        cx.background_spawn(async { cache::tidy() }).detach();
        let server = client.server().to_string();
        persist::update(|state| {
            let team_id = state
                .session
                .as_ref()
                .filter(|s| s.server == server)
                .and_then(|s| s.team_id.clone());
            state.session = Some(SavedSession {
                server: server.clone(),
                token: client.token().to_string(),
                team_id,
                user: Some(me.clone()),
            });
            state.last_server = server.clone();
        });
        let workspace = cx.new(|cx| Workspace::new(client, me, team, window, cx));
        self._screen_subscription = Some(cx.subscribe_in(
            &workspace,
            window,
            |this, _, event, window, cx| match event {
                WorkspaceEvent::SignedOut => {
                    end_session();
                    this.show_login(
                        Some("Your session has expired. Sign in again.".into()),
                        window,
                        cx,
                    );
                }
            },
        ));
        self.screen = Screen::SignedIn(workspace);
        cx.notify();
    }

    fn show_settings(&mut self, _: &ShowSettings, window: &mut Window, cx: &mut Context<Self>) {
        settings::open(window, cx);
    }

    fn sign_out(&mut self, _: &SignOut, window: &mut Window, cx: &mut Context<Self>) {
        let Screen::SignedIn(workspace) = &self.screen else {
            return;
        };
        let client = workspace.read(cx).client(cx);
        cx.background_spawn(async move {
            let _ = client.logout().await;
        })
        .detach();
        end_session();
        self.show_login(None, window, cx);
    }
}

/// Forget the saved session and what was cached for it, whether the user
/// signed out or the server ended the session. The account's pictures,
/// attachment previews, and contacts shouldn't outlast it on this
/// computer, nor greet whoever signs in next.
fn end_session() {
    persist::update(|state| state.session = None);
    cache::clear();
}

impl Render for AppRoot {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(Self::sign_out))
            .on_action(cx.listener(Self::show_settings))
            .map(|this| match &self.screen {
                // The shape of the workspace about to open, so nothing
                // jumps when it does.
                Screen::Restoring => this.child(skeleton::workspace(cx)),
                Screen::SignedOut(login) => this
                    .child(
                        TitleBar::new()
                            .child(div().flex_1())
                            .children(update::title_bar_button(cx).map(|b| div().pr_2().child(b))),
                    )
                    .child(login.clone()),
                Screen::SignedIn(workspace) => this.child(workspace.clone()),
            })
    }
}
