//! Sign-in: server address plus password or personal access token.

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt as _,
    alert::Alert,
    button::{Button, ButtonVariants as _},
    form::{Field, Form},
    input::{Input, InputContentType, InputEvent, InputState},
    scroll::ScrollableElement as _,
    switch::Switch,
    v_flex,
};
use gpui_kit::{
    AppContext as _, Context, Entity, EventEmitter, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div,
    prelude::FluentBuilder as _,
};
use mattermost::{Client, User};

use crate::{
    persist,
    sso::{SsoEvent, SsoPanel},
};

pub enum LoginEvent {
    SignedIn { client: Client, me: User },
}

pub struct LoginView {
    backdrop: Entity<crate::dither::DitherView>,
    server: Entity<InputState>,
    login: Entity<InputState>,
    password: Entity<InputState>,
    mfa: Entity<InputState>,
    token: Entity<InputState>,
    /// Browser sign-in options for the typed server.
    sso: Entity<SsoPanel>,
    use_token: bool,
    needs_mfa: bool,
    submitting: bool,
    error: Option<SharedString>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<LoginEvent> for LoginView {}

impl LoginView {
    pub fn new(error: Option<SharedString>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let saved = persist::load();
        let server = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("chat.example.com")
                .default_value(saved.last_server.trim_end_matches('/').to_string())
        });
        let login = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Email or username")
                .default_value(saved.last_login.clone())
        });
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        let mfa = cx.new(|cx| InputState::new(window, cx).placeholder("6-digit code"));
        let token = cx.new(|cx| InputState::new(window, cx).masked(true));

        let mut subscriptions: Vec<Subscription> = [&server, &login, &password, &mfa, &token]
            .into_iter()
            .map(|input| {
                cx.subscribe_in(input, window, |this, _, event, window, cx| {
                    if let InputEvent::PressEnter { .. } = event {
                        this.submit(window, cx);
                    }
                })
            })
            .collect();

        let sso = cx.new(|cx| SsoPanel::new(window, cx));
        let initial_server = server.read(cx).value().to_string();
        sso.update(cx, |sso, cx| sso.set_server(&initial_server, cx));
        subscriptions.push(cx.subscribe(&server, |this, server, event, cx| {
            if matches!(event, InputEvent::Change) {
                // An error about the previous address no longer applies.
                this.error = None;
                let value = server.read(cx).value().to_string();
                this.sso.update(cx, |sso, cx| sso.set_server(&value, cx));
            }
        }));
        subscriptions.push(cx.subscribe(&sso, |_, _, event, cx| match event {
            SsoEvent::SignedIn { client, me } => cx.emit(LoginEvent::SignedIn {
                client: client.clone(),
                me: me.clone(),
            }),
        }));
        subscriptions.push(cx.observe(&sso, |_, _, cx| cx.notify()));

        Self {
            backdrop: cx.new(|_| crate::dither::DitherView::new()),
            server,
            login,
            password,
            mfa,
            token,
            sso,
            use_token: false,
            needs_mfa: false,
            submitting: false,
            error,
            _subscriptions: subscriptions,
        }
    }

    /// Focus the first field that still needs input.
    pub fn focus_first_field(&self, window: &mut Window, cx: &mut Context<Self>) {
        let target = if self.server.read(cx).value().is_empty() {
            &self.server
        } else if self.use_token {
            &self.token
        } else if self.login.read(cx).value().is_empty() {
            &self.login
        } else {
            &self.password
        };
        target.focus_handle(cx).focus(window, cx);
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let sso = self.sso.read(cx);
        let password_hidden = !sso.allows_password() || sso.is_waiting();
        if self.submitting || (!self.use_token && password_hidden) {
            return;
        }
        let server = self.server.read(cx).value().to_string();
        let login = self.login.read(cx).value().to_string();
        let password = self.password.read(cx).value().to_string();
        let mfa = self.mfa.read(cx).value().to_string();
        let token = self.token.read(cx).value().to_string();
        let use_token = self.use_token;

        let missing = if server.trim().is_empty() {
            Some("Enter your server address.")
        } else if use_token && token.trim().is_empty() {
            Some("Paste a personal access token.")
        } else if !use_token && (login.trim().is_empty() || password.is_empty()) {
            Some("Enter your email or username and password.")
        } else {
            None
        };
        if let Some(message) = missing {
            self.error = Some(message.into());
            cx.notify();
            return;
        }

        self.submitting = true;
        self.error = None;
        cx.notify();
        persist::update(|state| state.last_login = login.trim().to_string());

        cx.spawn_in(window, async move |this, cx| {
            let result = if use_token {
                Client::with_token(&server, &token).await
            } else {
                let mfa = (!mfa.trim().is_empty()).then_some(mfa.as_str());
                Client::login(&server, &login, &password, mfa).await
            };
            let _ = this.update_in(cx, |this, window, cx| {
                this.submitting = false;
                match result {
                    Ok((client, me)) => cx.emit(LoginEvent::SignedIn { client, me }),
                    Err(err) => {
                        let message = format!("{err:#}");
                        let lower = message.to_lowercase();
                        if lower.contains("mfa") || lower.contains("multi-factor") {
                            this.needs_mfa = true;
                            this.mfa.focus_handle(cx).focus(window, cx);
                        }
                        this.error = Some(message.into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn set_use_token(&mut self, use_token: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.use_token = use_token;
        self.error = None;
        self.focus_first_field(window, cx);
        cx.notify();
    }
}

impl Render for LoginView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let submitting = self.submitting;
        let sso = self.sso.read(cx);
        // While the browser handles sign-in, or when the server has no
        // password sign-in, the password form steps aside.
        let password = !self.use_token && sso.allows_password() && !sso.is_waiting();
        let can_submit = self.use_token || password;
        let form = v_flex()
            .my_auto()
            .flex_none()
            .relative()
            .w(gpui_kit::rems(28.))
            .max_w_full()
            .p_8()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .gap_6()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(theme.muted_foreground)
                    .child("mtc / Mattermost"),
            )
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_xl().font_semibold().child("Your team, in reach"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Sign in with your Mattermost server and account."),
                    ),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(Alert::error("login-error", error))
            })
            .child(
                Form::new()
                    .child(
                        Field::new().label("Server").child(
                            Input::new(&self.server)
                                .id("login-server")
                                .disabled(submitting),
                        ),
                    )
                    .child(
                        Field::new()
                            .label_indent(false)
                            .visible(!self.use_token)
                            .child(self.sso.clone()),
                    )
                    .child(
                        Field::new()
                            .label("Email or username")
                            .visible(password)
                            .child(Input::new(&self.login).disabled(submitting)),
                    )
                    .child(
                        Field::new().label("Password").visible(password).child(
                            Input::new(&self.password)
                                .content_type(InputContentType::Password)
                                .mask_toggle()
                                .disabled(submitting),
                        ),
                    )
                    .child(
                        Field::new()
                            .label("Authentication code")
                            .visible(password && self.needs_mfa)
                            .child(Input::new(&self.mfa).disabled(submitting)),
                    )
                    .child(
                        Field::new()
                            .label("Personal access token")
                            .description("Create one under Profile › Security in Mattermost.")
                            .visible(self.use_token)
                            .child(
                                Input::new(&self.token)
                                    .content_type(InputContentType::Password)
                                    .mask_toggle()
                                    .disabled(submitting),
                            ),
                    )
                    .child(
                        Field::new().label_indent(false).child(
                            Switch::new("use-token")
                                .label("Use an access token")
                                .small()
                                .checked(self.use_token)
                                .disabled(submitting)
                                .on_change(cx.listener(|this, value, window, cx| {
                                    this.set_use_token(*value, window, cx)
                                })),
                        ),
                    )
                    .when(can_submit, |form| {
                        form.footer(
                            Button::new("sign-in")
                                .primary()
                                .label("Sign in")
                                .w_full()
                                .loading(submitting)
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                ),
                        )
                    }),
            );
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(div().absolute().inset_0().child(self.backdrop.clone()))
            .child(
                v_flex()
                    .id("login-scroll")
                    .size_full()
                    .overflow_y_scrollbar()
                    .child(
                        v_flex()
                            .min_h_full()
                            .flex_none()
                            .items_center()
                            .p_6()
                            .child(form),
                    ),
            )
    }
}
