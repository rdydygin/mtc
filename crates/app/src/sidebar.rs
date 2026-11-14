//! Channel navigation: team switcher, the Threads pane, favorites, channels,
//! direct messages, and the signed-in account.

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Icon, Selectable as _, Sizable as _, StyledExt as _, WindowExt as _,
    avatar::Avatar,
    button::{Button, ButtonVariant, ButtonVariants as _},
    h_flex,
    menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem},
    popover::Popover,
    separator::Separator,
    sidebar::SidebarHeader,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _,
};
use mattermost::{ChannelId, ChannelType, Status, TeamId};
use session::{ChannelEntry, Connection, Session};

use crate::{
    actions::{
        QUICK_SWITCH_KEY, QuickSwitch, ShowSettings, ShowShortcuts, SignOut, UseDarkAppearance,
        UseLightAppearance, UseSystemAppearance, shortcut_label,
    },
    http::avatar_url,
    persist::{self, Appearance},
    time,
};

/// Direct messages shown when they are neither unread nor recent.
const DIRECT_MESSAGE_LIMIT: usize = 20;

pub enum SidebarEvent {
    OpenChannel(ChannelId),
    OpenThreads,
}

/// The channels in sidebar order, as three sections. A favorite appears
/// only under favorites, whatever its kind.
pub struct ChannelSections {
    pub favorites: Vec<ChannelId>,
    pub channels: Vec<ChannelId>,
    pub direct: Vec<ChannelId>,
}

impl ChannelSections {
    /// Every channel in the order shown, for next/previous navigation.
    pub fn ordered(&self) -> impl Iterator<Item = &ChannelId> {
        self.favorites
            .iter()
            .chain(self.channels.iter())
            .chain(self.direct.iter())
    }
}

pub fn sections(session: &Session, cx: &App) -> ChannelSections {
    let mut sections = conversation_sections(session, cx);
    let active = session.active_channel();
    let mut ix = 0;
    sections.direct.retain(|id| {
        let recent = ix < DIRECT_MESSAGE_LIMIT;
        ix += 1;
        recent
            || Some(id) == active
            || session
                .channel(id)
                .is_some_and(|e| e.is_unread() || e.mention_count() > 0)
    });
    sections
}

/// Every conversation, grouped for navigation before the sidebar's limit.
pub fn conversation_sections(session: &Session, cx: &App) -> ChannelSections {
    let mut favorites: Vec<(&ChannelEntry, String)> = Vec::new();
    let mut channels: Vec<(&ChannelEntry, String)> = Vec::new();
    let mut direct: Vec<&ChannelEntry> = Vec::new();
    for entry in session.channels() {
        if session.is_favorite(entry.id()) {
            favorites.push((entry, session.channel_label(entry, cx).to_lowercase()));
        } else if entry.kind().is_direct_or_group() {
            direct.push(entry);
        } else {
            channels.push((entry, session.channel_label(entry, cx).to_lowercase()));
        }
    }
    favorites.sort_by(|a, b| a.1.cmp(&b.1));
    channels.sort_by(|a, b| a.1.cmp(&b.1));
    direct.sort_by_key(|e| std::cmp::Reverse(e.last_activity()));
    ChannelSections {
        favorites: favorites.into_iter().map(|(e, _)| e.id().clone()).collect(),
        channels: channels.into_iter().map(|(e, _)| e.id().clone()).collect(),
        direct: direct.into_iter().map(|e| e.id().clone()).collect(),
    }
}

pub struct SidebarView {
    session: Entity<Session>,
    /// The Threads pane is shown, rather than a channel.
    threads_selected: bool,
    favorites_collapsed: bool,
    channels_collapsed: bool,
    direct_collapsed: bool,
    account_open: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl SidebarView {
    pub fn new(session: Entity<Session>, cx: &mut Context<Self>) -> Self {
        let directory = session.read(cx).directory().clone();
        let threads = session.read(cx).threads().clone();
        let subscriptions = vec![
            cx.observe(&session, |_, _, cx| cx.notify()),
            cx.observe(&directory, |_, _, cx| cx.notify()),
            cx.observe(&threads, |_, _, cx| cx.notify()),
        ];
        Self {
            session,
            threads_selected: false,
            favorites_collapsed: false,
            channels_collapsed: false,
            direct_collapsed: false,
            account_open: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_threads_selected(&mut self, selected: bool, cx: &mut Context<Self>) {
        if self.threads_selected != selected {
            self.threads_selected = selected;
            cx.notify();
        }
    }

    fn status_color(status: Status, cx: &App) -> Hsla {
        crate::status::presence_color(status, cx)
    }

    fn channel_item(&self, id: &ChannelId, cx: &mut Context<Self>) -> Option<AnyElement> {
        let session = self.session.read(cx);
        let entry = session.channel(id)?;
        let label = session.channel_label(entry, cx);
        let custom_emoji = session.custom_emoji().clone();
        let theme = cx.theme();
        let unread = entry.is_unread() && !entry.is_muted();
        let mentions = entry.mention_count();
        let active = session.active_channel() == Some(id);
        let mut presence = String::new();
        let mut custom_status = None;
        let leading = if let Some(partner) = entry.channel().direct_partner(&session.me().id) {
            let directory = session.directory().read(cx);
            custom_status = directory
                .user(&partner)
                .and_then(|u| u.custom_status(&time::now_rfc3339()));
            let status = directory.status(&partner);
            presence = format!(
                ", {}",
                match status {
                    Status::Online => "online",
                    Status::Away => "away",
                    Status::DoNotDisturb => "do not disturb",
                    Status::Offline => "offline",
                }
            );
            let mut avatar = Avatar::new().name(label.clone()).size_6();
            if let Some(user) = directory.user(&partner) {
                avatar = avatar.src(avatar_url(
                    session.client(),
                    partner.as_str(),
                    user.last_picture_update,
                ));
            }
            div()
                .relative()
                .size_6()
                .flex_none()
                .child(avatar)
                .child(
                    div()
                        .absolute()
                        .bottom_0()
                        .right_0()
                        .size_2()
                        .rounded_full()
                        .border_1()
                        .border_color(theme.background)
                        .bg(Self::status_color(status, cx)),
                )
                .into_any_element()
        } else {
            let icon = match entry.kind() {
                ChannelType::Private => IconName::Lock,
                ChannelType::Group => IconName::Users,
                _ => IconName::Hash,
            };
            div()
                .size_6()
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(theme.radius)
                .bg(if active {
                    theme.primary.opacity(0.12)
                } else {
                    theme.muted
                })
                .child(Icon::new(icon).small().text_color(if active {
                    theme.primary
                } else {
                    theme.muted_foreground
                }))
                .into_any_element()
        };
        let channel = id.clone();
        Some(
            Button::new(SharedString::from(format!("channel-{id}")))
                .ghost()
                .w_full()
                .h_9()
                .px_2()
                .selected(active)
                .accessibility_label(format!(
                    "{label}{presence}{}",
                    if unread { ", unread" } else { "" }
                ))
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(leading)
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_sm()
                                .text_color(if active || unread || mentions > 0 {
                                    theme.foreground
                                } else {
                                    theme.muted_foreground
                                })
                                .when(unread || mentions > 0, |this| this.font_semibold())
                                .child(label),
                        )
                        .when_some(custom_status, |this, status| {
                            this.child(
                                div()
                                    .flex_none()
                                    .text_sm()
                                    .child(crate::status::emoji_element(
                                        &status.emoji,
                                        &custom_emoji,
                                    )),
                            )
                        })
                        .child(
                            div()
                                .w_6()
                                .flex_none()
                                .text_right()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(if mentions > 0 {
                                    mentions.min(99).to_string()
                                } else if unread {
                                    "•".into()
                                } else {
                                    String::new()
                                }),
                        ),
                )
                .on_click(cx.listener({
                    let channel = channel.clone();
                    move |_, _, _, cx| cx.emit(SidebarEvent::OpenChannel(channel.clone()))
                }))
                .context_menu({
                    let session = self.session.clone();
                    move |menu, _, cx| {
                        let session_ref = session.read(cx);
                        let favorite = session_ref.is_favorite(&channel);
                        let can_leave = session_ref
                            .channel(&channel)
                            .is_some_and(ChannelEntry::can_leave);
                        let session = session.clone();
                        let channel = channel.clone();
                        let leave = (session.clone(), channel.clone());
                        menu.item(PopupMenuItem::new(favorite_label(favorite)).on_click(
                            move |_, _, cx| {
                                session.update(cx, |s, cx| s.set_favorite(&channel, !favorite, cx));
                            },
                        ))
                        .when(can_leave, move |menu| {
                            let (session, channel) = leave;
                            menu.separator()
                                .item(PopupMenuItem::new("Leave channel").on_click(
                                    move |_, window, cx| {
                                        confirm_leave(&session, &channel, window, cx)
                                    },
                                ))
                        })
                    }
                })
                .into_any_element(),
        )
    }

    /// The Threads pane's entry, above the conversations, in the shape of a
    /// channel row.
    fn threads_item(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let threads = self.session.read(cx).threads().read(cx);
        let (unread, mentions) = (threads.unread_threads() > 0, threads.unread_mentions());
        let active = self.threads_selected;
        let theme = cx.theme();
        Button::new("threads")
            .ghost()
            .w_full()
            .h_9()
            .px_2()
            .selected(active)
            .accessibility_label(match (mentions, unread) {
                (0, false) => "Threads".to_string(),
                (0, true) => "Threads, unread".to_string(),
                (1, _) => "Threads, 1 mention".to_string(),
                (mentions, _) => format!("Threads, {mentions} mentions"),
            })
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(
                        div()
                            .size_6()
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(theme.radius)
                            .bg(if active {
                                theme.primary.opacity(0.12)
                            } else {
                                theme.muted
                            })
                            .child(Icon::new(IconName::MessagesSquare).small().text_color(
                                if active {
                                    theme.primary
                                } else {
                                    theme.muted_foreground
                                },
                            )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(if active || unread {
                                theme.foreground
                            } else {
                                theme.muted_foreground
                            })
                            .when(unread, |this| this.font_semibold())
                            .child("Threads"),
                    )
                    .child(
                        div()
                            .w_6()
                            .flex_none()
                            .text_right()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if mentions > 0 {
                                mentions.min(99).to_string()
                            } else if unread {
                                "•".into()
                            } else {
                                String::new()
                            }),
                    ),
            )
            .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::OpenThreads)))
    }

    /// The heading of a collapsible section of the conversation list.
    fn section_header(
        id: &'static str,
        label: &'static str,
        accessibility_label: &'static str,
        collapsed: bool,
        cx: &App,
    ) -> Button {
        Button::new(id)
            .ghost()
            .w_full()
            .justify_start()
            .small()
            .accessibility_label(accessibility_label)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(
                        Icon::new(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall(),
                    )
                    .child(label),
            )
    }

    fn render_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        v_flex()
            .w(gpui_kit::rems(3.5))
            .flex_none()
            .items_center()
            .py_3()
            .gap_3()
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child("mtc"),
            )
            .children(session.teams().iter().map(|team| {
                let id = team.id.clone();
                let current = session.team().is_some_and(|t| t.id == id);
                let initials: String = team
                    .display_name
                    .split_whitespace()
                    .filter_map(|w| w.chars().next())
                    .take(2)
                    .collect();
                let session = self.session.clone();
                Button::new(SharedString::from(format!("team-{id}")))
                    .ghost()
                    .size_10()
                    .selected(current)
                    .label(initials)
                    .tooltip(team.display_name.clone())
                    .accessibility_label(format!("Switch to {}", team.display_name))
                    .on_click(move |_, _, cx| {
                        session.update(cx, |s, cx| s.select_team(id.clone(), cx))
                    })
            }))
            .child(div().flex_1())
            .child(self.render_account(cx))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let teams_pending = !session.teams_loaded();
        let team_name: SharedString = session
            .team()
            .map(|t| t.display_name.clone())
            .unwrap_or_else(|| "Mattermost".to_string())
            .into();
        let teams: Vec<(TeamId, String, bool)> = session
            .teams()
            .iter()
            .map(|t| {
                let current = session.team().is_some_and(|c| c.id == t.id);
                (t.id.clone(), t.display_name.clone(), current)
            })
            .collect();
        let has_choice = teams.len() > 1;
        let session = self.session.clone();
        let team_button = Button::new("team")
            .ghost()
            .w_full()
            .justify_start()
            .font_semibold()
            .accessibility_label(team_name.clone())
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .map(|this| {
                        if teams_pending {
                            this.child(div().flex_1().child(crate::skeleton::team_name(cx)))
                        } else {
                            this.child(div().flex_1().min_w_0().truncate().child(team_name))
                        }
                    })
                    .when(has_choice, |this| {
                        this.child(Icon::new(IconName::ChevronsUpDown).small())
                    }),
            );
        let team_button: AnyElement = if has_choice {
            team_button
                .dropdown_menu(move |mut menu, _, _| {
                    for (id, name, current) in teams.iter().cloned() {
                        let session = session.clone();
                        menu = menu.item(PopupMenuItem::new(name).checked(current).on_click(
                            move |_, _, cx| {
                                let id = id.clone();
                                session.update(cx, |s, cx| s.select_team(id, cx));
                            },
                        ));
                    }
                    menu
                })
                .into_any_element()
        } else {
            team_button.into_any_element()
        };
        SidebarHeader::new().w_full().child(
            v_flex().w_full().gap_2().child(team_button).child(
                Button::new("jump-to")
                    .outline()
                    .small()
                    .w_full()
                    .justify_start()
                    .accessibility_label("Jump to a conversation")
                    .child(
                        // A search field, not a command: quiet secondary text.
                        h_flex()
                            .w_full()
                            .gap_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(Icon::new(IconName::Search).xsmall())
                            .child(div().flex_1().child("Jump to…"))
                            .child(
                                div()
                                    .px_1()
                                    .rounded(cx.theme().radius)
                                    .border_1()
                                    .border_color(cx.theme().border)
                                    .child(shortcut_label(QUICK_SWITCH_KEY)),
                            ),
                    )
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(QuickSwitch), cx)),
            ),
        )
    }

    /// The signed-in user as a compact avatar; details and account
    /// controls open in a popover.
    fn render_account(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let session = self.session.read(cx);
        let me = session.me().clone();
        let avatar = avatar_url(session.client(), me.id.as_str(), me.last_picture_update);
        let status = session.directory().read(cx).status(&me.id);
        let connection = session.connection();
        let theme = cx.theme();
        let dot = Self::status_color(status, cx);
        let connection_label = match connection {
            Connection::Online => "Connected",
            Connection::Connecting => "Connecting…",
            Connection::Offline => "Reconnecting…",
        };
        let name = me.display_name();
        let trigger = Button::new("account")
            .ghost()
            .p_0()
            .size_10()
            .tooltip(name.clone())
            .accessibility_label(format!("Account: {name}, {connection_label}"))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .size_8()
                    .child(
                        Avatar::new()
                            .name(name.clone())
                            .src(avatar.clone())
                            .size_8(),
                    )
                    .child(
                        div()
                            .absolute()
                            .right_0()
                            .bottom_0()
                            .size_2p5()
                            .rounded_full()
                            .border_2()
                            .border_color(theme.muted)
                            .bg(dot),
                    ),
            );
        let entity = cx.entity().downgrade();
        let username = me.username.clone();
        let session = self.session.clone();
        Popover::new("account-popover")
            .anchor(gpui_kit::Anchor::BottomLeft)
            .open(self.account_open)
            .on_open_change({
                let entity = entity.clone();
                move |open, _, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.account_open = *open;
                        cx.notify();
                    });
                }
            })
            .trigger(trigger)
            .content(move |_, window, cx| {
                let theme = cx.theme().clone();
                let (connection_label, connection_color) = match connection {
                    Connection::Online => ("Connected", theme.success),
                    Connection::Connecting => ("Connecting…", theme.muted_foreground),
                    Connection::Offline => ("Reconnecting…", theme.warning),
                };
                // Read live so the popover reflects changes made from it.
                let (presence, custom) = {
                    let session = session.read(cx);
                    let directory = session.directory().read(cx);
                    let me = session.me().id.clone();
                    (
                        directory.status(&me),
                        directory
                            .user(&me)
                            .and_then(|u| u.custom_status(&time::now_rfc3339())),
                    )
                };
                let appearance = persist::load().appearance;
                let close = {
                    let entity = entity.clone();
                    move |cx: &mut App| {
                        let _ = entity.update(cx, |this, cx| {
                            this.account_open = false;
                            cx.notify();
                        });
                    }
                };
                let appearance_button =
                    |id: &'static str,
                     icon: IconName,
                     tooltip: &'static str,
                     name: &'static str,
                     value: Appearance| {
                        // A segmented single choice: each icon is a toggle
                        // whose pressed state is the current appearance.
                        Button::new(id)
                            .small()
                            .ghost()
                            .selected(appearance == value)
                            .toggled(appearance == value)
                            .icon(icon)
                            .accessibility_label(name)
                            .tooltip(tooltip)
                            .on_click(move |_, window, cx| {
                                let action: Box<dyn gpui_kit::Action> = match value {
                                    Appearance::System => Box::new(UseSystemAppearance),
                                    Appearance::Light => Box::new(UseLightAppearance),
                                    Appearance::Dark => Box::new(UseDarkAppearance),
                                };
                                window.dispatch_action(action, cx);
                            })
                    };
                let presence_row = |status: Status, label: &'static str| {
                    let session = session.clone();
                    let color = match status {
                        Status::Online => theme.success,
                        Status::Away => theme.warning,
                        Status::DoNotDisturb => theme.danger,
                        Status::Offline => theme.muted_foreground,
                    };
                    Button::new(SharedString::from(format!("presence-{}", status.as_str())))
                        .small()
                        .ghost()
                        // Plain text children are not exposed to assistive
                        // technology, so the row names itself; the check mark
                        // is mirrored as the pressed state.
                        .accessibility_label(label)
                        .toggled(presence == status)
                        .w_full()
                        .justify_start()
                        .child(
                            h_flex()
                                .w_full()
                                .gap_2()
                                .child(div().size_2().rounded_full().bg(color))
                                .child(div().flex_1().child(label))
                                .when(presence == status, |this| {
                                    this.child(Icon::new(IconName::Check).small())
                                }),
                        )
                        .on_click(move |_, _, cx| {
                            session.update(cx, |s, cx| s.set_presence(status, cx));
                        })
                };
                let set_status = {
                    let close = close.clone();
                    let session = session.clone();
                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                        close(cx);
                        crate::status::open(session.clone(), window, cx);
                    }
                };
                let settings = close.clone();
                let shortcuts = close.clone();
                let sign_out = close.clone();
                let _ = window;
                v_flex()
                    .w_64()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_3()
                            .child(
                                Avatar::new()
                                    .name(name.clone())
                                    .src(avatar.clone())
                                    .size_10(),
                            )
                            .child(
                                v_flex()
                                    .min_w_0()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_semibold()
                                            .truncate()
                                            .child(name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .truncate()
                                            .child(format!("@{username}")),
                                    )
                                    .child(
                                        h_flex()
                                            .gap_1()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(
                                                div()
                                                    .size_1p5()
                                                    .rounded_full()
                                                    .bg(connection_color),
                                            )
                                            .child(connection_label),
                                    ),
                            ),
                    )
                    .child(
                        // The custom status others see beside your name.
                        Button::new("custom-status")
                            .small()
                            .outline()
                            .w_full()
                            .justify_start()
                            .accessibility_label(match &custom {
                                Some(status) if !status.text.is_empty() => {
                                    format!("Change status: {}", status.text)
                                }
                                Some(status) => {
                                    format!(
                                        "Change status: :{}:",
                                        session::emoji_name(&status.emoji)
                                    )
                                }
                                None => "Set a status".to_string(),
                            })
                            .child(match &custom {
                                Some(status) => h_flex()
                                    .w_full()
                                    .gap_2()
                                    .child(crate::status::emoji_element(
                                        &status.emoji,
                                        session.read(cx).custom_emoji(),
                                    ))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(status.text.clone()),
                                    ),
                                None => h_flex()
                                    .w_full()
                                    .gap_2()
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(IconName::FaceSlightlySmilingPlus).small())
                                    .child("Set a status…"),
                            })
                            .on_click(set_status),
                    )
                    .child(
                        v_flex()
                            .child(presence_row(Status::Online, "Online"))
                            .child(presence_row(Status::Away, "Away"))
                            .child(presence_row(Status::DoNotDisturb, "Do not disturb"))
                            .child(presence_row(Status::Offline, "Appear offline")),
                    )
                    .child(Separator::horizontal())
                    .child(
                        h_flex()
                            .justify_between()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("Appearance"),
                            )
                            .child(
                                h_flex()
                                    .gap_0p5()
                                    .child(appearance_button(
                                        "appearance-system",
                                        IconName::Monitor,
                                        "Match system",
                                        "Match system appearance",
                                        Appearance::System,
                                    ))
                                    .child(appearance_button(
                                        "appearance-light",
                                        IconName::Sun,
                                        "Light",
                                        "Light appearance",
                                        Appearance::Light,
                                    ))
                                    .child(appearance_button(
                                        "appearance-dark",
                                        IconName::Moon,
                                        "Dark",
                                        "Dark appearance",
                                        Appearance::Dark,
                                    )),
                            ),
                    )
                    .child(Separator::horizontal())
                    .child(
                        v_flex()
                            .child(
                                Button::new("account-settings")
                                    .small()
                                    .ghost()
                                    .accessibility_label("Settings…")
                                    .w_full()
                                    .justify_start()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .child(Icon::new(IconName::Settings).small())
                                            .child("Settings…"),
                                    )
                                    .on_click(move |_, window, cx| {
                                        settings(cx);
                                        window.dispatch_action(Box::new(ShowSettings), cx);
                                    }),
                            )
                            .child(
                                Button::new("account-shortcuts")
                                    .small()
                                    .ghost()
                                    .accessibility_label("Keyboard shortcuts")
                                    .w_full()
                                    .justify_start()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .child(Icon::new(IconName::Keyboard).small())
                                            .child("Keyboard shortcuts"),
                                    )
                                    .on_click(move |_, window, cx| {
                                        shortcuts(cx);
                                        window.dispatch_action(Box::new(ShowShortcuts), cx);
                                    }),
                            )
                            .child(
                                Button::new("account-sign-out")
                                    .small()
                                    .ghost()
                                    .accessibility_label("Sign out")
                                    .w_full()
                                    .justify_start()
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .gap_2()
                                            .child(Icon::new(IconName::LogOut).small())
                                            .child("Sign out"),
                                    )
                                    .on_click(move |_, window, cx| {
                                        sign_out(cx);
                                        window.dispatch_action(Box::new(SignOut), cx);
                                    }),
                            ),
                    )
            })
    }
}

impl Render for SidebarView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sections = sections(self.session.read(cx), cx);
        let favorites: Vec<_> = sections
            .favorites
            .iter()
            .filter_map(|id| self.channel_item(id, cx))
            .collect();
        let channels: Vec<_> = sections
            .channels
            .iter()
            .filter_map(|id| self.channel_item(id, cx))
            .collect();
        let direct: Vec<_> = sections
            .direct
            .iter()
            .filter_map(|id| self.channel_item(id, cx))
            .collect();
        // Until the channel list arrives, each section shows rows in the
        // shape of the conversations about to fill it.
        let state = self.session.read(cx).channels_state().clone();
        let loading = matches!(
            state,
            session::LoadState::NotLoaded | session::LoadState::Loading
        );
        let empty = state == session::LoadState::Loaded
            && self.session.read(cx).channels().next().is_none();
        let theme = cx.theme().clone();
        // The workspace paints the backdrop; the sidebar is one of its
        // inset panels.
        h_flex()
            .size_full()
            .items_stretch()
            .child(self.render_rail(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .my_2()
                    .mr_2()
                    .rounded(theme.radius_lg)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(self.render_header(cx))
                    .when(
                        self.session.read(cx).connection() != Connection::Online,
                        |this| {
                            this.child(
                                h_flex()
                                    .px_3()
                                    .py_2()
                                    .gap_2()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(IconName::RefreshCw).xsmall())
                                    .child(
                                        if self.session.read(cx).connection()
                                            == Connection::Connecting
                                        {
                                            "Connecting…"
                                        } else {
                                            "Reconnecting…"
                                        },
                                    ),
                            )
                        },
                    )
                    .child(
                        v_flex()
                            .id("conversation-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .px_2()
                            .gap_0p5()
                            .child(self.threads_item(cx))
                            .when_some(match state { session::LoadState::Failed(message) => Some(message), _ => None }, |this, message| {
                                this.child(crate::recovery::inline_failure(message,
                                    Button::new("retry-conversations").on_click(cx.listener(|this, _, _, cx| {
                                        this.session.update(cx, |s, cx| s.retry_channels(cx));
                                    })), cx))
                            })
                            .when(empty, |this| this.child(div().p_3().text_sm().text_color(theme.muted_foreground).child("No conversations yet. Join a channel in Mattermost to get started.")))
                            .when(!favorites.is_empty(), |this| {
                                this.child(
                                    Self::section_header(
                                        "favorites-section",
                                        "FAVORITES",
                                        "Toggle favorites",
                                        self.favorites_collapsed,
                                        cx,
                                    )
                                    .mt_4()
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.favorites_collapsed = !this.favorites_collapsed;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .when(!self.favorites_collapsed, |this| this.children(favorites))
                            })
                            .child(
                                Self::section_header(
                                    "channels-section",
                                    "CHANNELS",
                                    "Toggle channels",
                                    self.channels_collapsed,
                                    cx,
                                )
                                .mt_4()
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.channels_collapsed = !this.channels_collapsed;
                                        cx.notify();
                                    },
                                )),
                            )
                            .when(!self.channels_collapsed, |this| {
                                this.children(channels).when(loading, |this| {
                                    this.child(crate::skeleton::conversation_rows(6, 0, cx))
                                })
                            })
                            .child(
                                Self::section_header(
                                    "direct-section",
                                    "DIRECT MESSAGES",
                                    "Toggle direct messages",
                                    self.direct_collapsed,
                                    cx,
                                )
                                .mt_4()
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.direct_collapsed = !this.direct_collapsed;
                                        cx.notify();
                                    },
                                )),
                            )
                            .when(!self.direct_collapsed, |this| {
                                this.children(direct).when(loading, |this| {
                                    this.child(crate::skeleton::conversation_rows(4, 6, cx))
                                })
                            }),
                    )
                    .pb_2(),
            )
    }
}

/// The command that adds a conversation to favorites or removes it, worded
/// the same in every menu.
pub fn favorite_label(favorite: bool) -> &'static str {
    if favorite {
        "Remove from favorites"
    } else {
        "Add to favorites"
    }
}

/// Ask before leaving `channel`, then leave it. mtc can't browse for
/// channels to join, so coming back takes another app or an invitation.
pub fn confirm_leave(
    session: &Entity<Session>,
    channel: &ChannelId,
    window: &mut Window,
    cx: &mut App,
) {
    let session_ref = session.read(cx);
    let Some(entry) = session_ref.channel(channel).filter(|e| e.can_leave()) else {
        return;
    };
    let title = format!("Leave {}?", session_ref.channel_label(entry, cx));
    let description = if entry.kind() == ChannelType::Private {
        "This channel is private. Ask someone in it to add you back."
    } else {
        "You can rejoin from Mattermost in your browser, or ask someone to add you back."
    };
    let session = session.clone();
    let channel = channel.clone();
    window.open_alert_dialog(cx, move |alert, _, _| {
        let session = session.clone();
        let channel = channel.clone();
        alert
            .title(title.clone())
            .description(description)
            .confirm()
            .ok_text("Leave")
            .ok_variant(ButtonVariant::Danger)
            .on_ok(move |_, _, cx| {
                session.update(cx, |s, cx| s.leave_channel(&channel, cx));
                true
            })
    });
}
