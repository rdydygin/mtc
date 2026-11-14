//! Channel navigation: team switcher, the Threads pane, favorites, channels,
//! direct messages, and the signed-in account.

use std::{collections::HashMap, rc::Rc};

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
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Hsla, InteractiveElement as _,
    IntoElement, ListAlignment, ListOffset, ListState, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Window, div, list, prelude::FluentBuilder as _, px,
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

gpui_kit::actions!(sidebar, [FocusNext, FocusPrevious]);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        gpui_kit::KeyBinding::new("tab", FocusNext, Some("Sidebar")),
        gpui_kit::KeyBinding::new("shift-tab", FocusPrevious, Some("Sidebar")),
    ]);
}

pub enum SidebarEvent {
    OpenChannel(ChannelId),
    OpenThreads,
}

/// Retained sidebar order, also used by next/previous navigation.
pub fn sections(session: &Session, cx: &App) -> std::rc::Rc<session::ChannelSections> {
    session.sidebar_sections(cx)
}

/// All conversations, including older read DMs available in the switcher.
pub fn conversation_sections(session: &Session, cx: &App) -> std::rc::Rc<session::ChannelSections> {
    session.conversation_sections(cx)
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SidebarRow {
    Threads,
    Failure(SharedString),
    Empty,
    Header(Section),
    Channel(ChannelId),
    Skeleton(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Section {
    Favorites,
    Channels,
    Direct,
}

struct SidebarRows {
    sections: Rc<session::ChannelSections>,
    collapsed: [bool; 3],
    state: session::LoadState,
    empty: bool,
    rows: Vec<SidebarRow>,
}

impl SidebarRows {
    fn new(
        sections: Rc<session::ChannelSections>,
        collapsed: [bool; 3],
        state: session::LoadState,
        empty: bool,
    ) -> Self {
        let mut rows = vec![SidebarRow::Threads];
        if let session::LoadState::Failed(message) = &state {
            rows.push(SidebarRow::Failure(message.clone()));
        }
        if empty {
            rows.push(SidebarRow::Empty);
        }
        let loading = matches!(
            state,
            session::LoadState::NotLoaded | session::LoadState::Loading
        );
        for (ix, (section, ids)) in [
            (Section::Favorites, sections.favorites()),
            (Section::Channels, sections.channels()),
            (Section::Direct, sections.direct()),
        ]
        .into_iter()
        .enumerate()
        {
            if ix == 0 && ids.is_empty() {
                continue;
            }
            rows.push(SidebarRow::Header(section));
            if collapsed[ix] {
                continue;
            }
            rows.extend(ids.iter().cloned().map(SidebarRow::Channel));
            if loading && ix > 0 {
                rows.extend((if ix == 1 { 0..6 } else { 6..10 }).map(SidebarRow::Skeleton));
            }
        }
        Self {
            sections,
            collapsed,
            state,
            empty,
            rows,
        }
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
    list: ListState,
    rows: Option<SidebarRows>,
    row_focus: HashMap<SidebarRow, FocusHandle>,
    list_start: FocusHandle,
    list_end: FocusHandle,
    #[cfg(test)]
    channel_rows_built: std::cell::Cell<usize>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SidebarEvent> for SidebarView {}

impl SidebarView {
    pub fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let directory = session.read(cx).directory().clone();
        let threads = session.read(cx).threads().clone();
        let list_start = cx.focus_handle().tab_stop(true);
        let list_end = cx.focus_handle().tab_stop(true);
        let subscriptions = vec![
            cx.on_focus(&list_start, window, |this, window, cx| {
                this.enter_list(false, window, cx)
            }),
            cx.on_focus(&list_end, window, |this, window, cx| {
                this.enter_list(true, window, cx)
            }),
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
            list: ListState::new(0, ListAlignment::Top, px(200.)),
            rows: None,
            row_focus: HashMap::new(),
            list_start,
            list_end,
            #[cfg(test)]
            channel_rows_built: std::cell::Cell::new(0),
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
        #[cfg(test)]
        self.channel_rows_built
            .set(self.channel_rows_built.get() + 1);
        let session = self.session.read(cx);
        let entry = session.channel(id)?;
        let label = session.channel_label(entry, cx);
        let custom_emoji = session.custom_emoji().clone();
        let theme = cx.theme();
        let unread = entry.is_unread() && !entry.is_muted();
        let mentions = entry.mention_count();
        let muted = entry.is_muted();
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
                .track_focus(&self.row_focus[&SidebarRow::Channel(id.clone())])
                .ghost()
                .w_full()
                .h_9()
                .px_2()
                .selected(active)
                .accessibility_label(format!(
                    "{label}{presence}{}{}",
                    if unread { ", unread" } else { "" },
                    if muted { ", muted" } else { "" }
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
                        .when(muted, |this| {
                            this.child(
                                Icon::new(IconName::BellOff)
                                    .small()
                                    .text_color(theme.muted_foreground),
                            )
                        })
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
                        let muted = session_ref
                            .channel(&channel)
                            .is_some_and(ChannelEntry::is_muted);
                        let mute_pending = session_ref.is_mute_pending(&channel);
                        let can_leave = session_ref
                            .channel(&channel)
                            .is_some_and(ChannelEntry::can_leave);
                        let session = session.clone();
                        let channel = channel.clone();
                        let leave = (session.clone(), channel.clone());
                        let mute = (session.clone(), channel.clone());
                        menu.item(PopupMenuItem::new(favorite_label(favorite)).on_click(
                            move |_, _, cx| {
                                session.update(cx, |s, cx| s.set_favorite(&channel, !favorite, cx));
                            },
                        ))
                        .item(
                            PopupMenuItem::new(mute_label(muted))
                                .disabled(mute_pending)
                                .on_click(move |_, _, cx| {
                                    let (session, channel) = &mute;
                                    session.update(cx, |s, cx| {
                                        s.set_channel_muted(channel, !muted, cx)
                                    });
                                }),
                        )
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
            .track_focus(&self.row_focus[&SidebarRow::Threads])
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

impl SidebarView {
    fn sync_rows(&mut self, cx: &mut Context<Self>) {
        let session = self.session.read(cx);
        let sections = sections(session, cx);
        let state = session.channels_state().clone();
        let empty = state == session::LoadState::Loaded && session.channels().next().is_none();
        let collapsed = [
            self.favorites_collapsed,
            self.channels_collapsed,
            self.direct_collapsed,
        ];
        if self.rows.as_ref().is_some_and(|rows| {
            Rc::ptr_eq(&rows.sections, &sections)
                && rows.collapsed == collapsed
                && rows.state == state
                && rows.empty == empty
        }) {
            return;
        }
        let next = SidebarRows::new(sections, collapsed, state, empty);
        // Preserve the first visible domain object and its offset across reorders.
        let top = self.list.logical_scroll_top();
        let anchor = self.rows.as_ref().and_then(|old| {
            let positions: HashMap<_, _> = next
                .rows
                .iter()
                .enumerate()
                .map(|(ix, row)| (row, ix))
                .collect();
            old.rows
                .iter()
                .enumerate()
                .filter_map(|(old_ix, row)| positions.get(row).map(|&new_ix| (old_ix, new_ix)))
                // Prefer the successor on a tie, so removing the top row leaves
                // the following conversation at the same position in the viewport.
                .min_by_key(|(old_ix, _)| (old_ix.abs_diff(top.item_ix), *old_ix < top.item_ix))
                .map(|(old_ix, item_ix)| {
                    let height = self
                        .list
                        .bounds_for_item(old_ix)
                        .map(|bounds| bounds.size.height)
                        .unwrap_or(px(0.));
                    ListOffset {
                        item_ix,
                        offset_in_item: top
                            .offset_in_item
                            .max(px(0.))
                            .min((height - px(1.)).max(px(0.))),
                    }
                })
        });
        let old_focus = std::mem::take(&mut self.row_focus);
        self.row_focus = next
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row,
                    SidebarRow::Threads
                        | SidebarRow::Channel(_)
                        | SidebarRow::Header(_)
                        | SidebarRow::Failure(_)
                )
            })
            .map(|row| {
                (
                    row.clone(),
                    old_focus
                        .get(row)
                        .cloned()
                        .unwrap_or_else(|| cx.focus_handle()),
                )
            })
            .collect();
        self.list.splice_focusable(
            0..self.list.item_count(),
            next.rows.iter().map(|row| self.row_focus.get(row).cloned()),
        );
        if let Some(anchor) = anchor {
            self.list.scroll_to(anchor);
        }
        self.rows = Some(next);
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let row = &self
            .rows
            .as_ref()
            .expect("rows synchronized before layout")
            .rows[ix];
        let content = match row {
            SidebarRow::Threads => self.threads_item(cx).into_any_element(),
            SidebarRow::Channel(id) => self
                .channel_item(id, cx)
                .unwrap_or_else(|| div().into_any_element()),
            SidebarRow::Header(section) => {
                let (id, label, accessibility_label, collapsed) = match section {
                    Section::Favorites => (
                        "favorites-section",
                        "FAVORITES",
                        "Toggle favorites",
                        self.favorites_collapsed,
                    ),
                    Section::Channels => (
                        "channels-section",
                        "CHANNELS",
                        "Toggle channels",
                        self.channels_collapsed,
                    ),
                    Section::Direct => (
                        "direct-section",
                        "DIRECT MESSAGES",
                        "Toggle direct messages",
                        self.direct_collapsed,
                    ),
                };
                let section = *section;
                Self::section_header(id, label, accessibility_label, collapsed, cx)
                    .track_focus(&self.row_focus[row])
                    .mt_4()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let collapsed = match section {
                            Section::Favorites => &mut this.favorites_collapsed,
                            Section::Channels => &mut this.channels_collapsed,
                            Section::Direct => &mut this.direct_collapsed,
                        };
                        *collapsed = !*collapsed;
                        cx.notify();
                    }))
                    .into_any_element()
            }
            SidebarRow::Failure(message) => crate::recovery::inline_failure(
                message.clone(),
                Button::new("retry-conversations")
                    .track_focus(&self.row_focus[row])
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.session.update(cx, |s, cx| s.retry_channels(cx));
                    })),
                cx,
            )
            .into_any_element(),
            SidebarRow::Empty => div()
                .p_3()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No conversations yet. Join a channel in Mattermost to get started.")
                .into_any_element(),
            SidebarRow::Skeleton(offset) => {
                crate::skeleton::conversation_rows(1, *offset, cx).into_any_element()
            }
        };
        div()
            .w_full()
            .px_2()
            .when(ix > 0, |this| this.pt_0p5())
            .child(content)
            .into_any_element()
    }

    fn enter_list(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let boundary = if backwards {
            &self.list_end
        } else {
            &self.list_start
        };
        // Exiting passes through a boundary within one update. Ignore its
        // delayed focus event once the next external control already has focus.
        if !boundary.is_focused(window) {
            return;
        }
        let rows = &self.rows.as_ref().unwrap().rows;
        let target = if backwards {
            rows.iter()
                .rposition(|row| self.row_focus.contains_key(row))
        } else {
            rows.iter().position(|row| self.row_focus.contains_key(row))
        };
        if let Some(ix) = target {
            self.row_focus[&rows[ix]].focus(window, cx);
            // Entry must also work before the intervening rows have heights.
            self.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    fn focus_adjacent(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = &self.rows.as_ref().unwrap().rows;
        let Some(ix) = rows.iter().position(|row| {
            self.row_focus
                .get(row)
                .is_some_and(|focus| focus.is_focused(window))
        }) else {
            cx.propagate();
            return;
        };
        let next = if backwards {
            rows[..ix]
                .iter()
                .rposition(|row| self.row_focus.contains_key(row))
        } else {
            rows.iter()
                .enumerate()
                .skip(ix + 1)
                .find(|(_, row)| self.row_focus.contains_key(row))
                .map(|(ix, _)| ix)
        };
        if let Some(next) = next {
            self.row_focus[&rows[next]].focus(window, cx);
            if self.list.bounds_for_item(next).is_some() {
                self.list.scroll_to_reveal_item(next);
            } else {
                self.list.scroll_to(ListOffset {
                    item_ix: next,
                    offset_in_item: px(0.),
                });
            }
            cx.notify();
        } else if backwards {
            self.list_start.focus(window, cx);
            window.focus_prev(cx);
        } else {
            self.list_end.focus(window, cx);
            window.focus_next(cx);
        }
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_rows(cx);
        let view = cx.entity().downgrade();
        div()
            .id("conversation-list")
            .v_flex()
            .key_context("Sidebar")
            .flex_1()
            .min_h_0()
            .on_action(
                cx.listener(|this, _: &FocusNext, window, cx| {
                    this.focus_adjacent(false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &FocusPrevious, window, cx| {
                this.focus_adjacent(true, window, cx)
            }))
            .child(
                div()
                    .id("conversation-list-start")
                    .h_0()
                    .track_focus(&self.list_start),
            )
            .child(
                list(self.list.clone(), move |ix, _, cx| {
                    view.update(cx, |this, cx| this.render_row(ix, cx))
                        .unwrap_or_else(|_| div().into_any_element())
                })
                .size_full()
                .flex_1()
                .min_h_0(),
            )
            .child(
                div()
                    .id("conversation-list-end")
                    .h_0()
                    .track_focus(&self.list_end),
            )
    }
}

impl Render for SidebarView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(self.render_list(cx))
                    .pb_2(),
            )
    }
}

/// The mute command, worded the same in every menu.
pub fn mute_label(muted: bool) -> &'static str {
    if muted {
        "Unmute channel"
    } else {
        "Mute channel"
    }
}

/// The favorite command, worded the same in every menu.
pub fn favorite_label(favorite: bool) -> &'static str {
    if favorite {
        "Remove from favorites"
    } else {
        "Add to favorites"
    }
}

/// Ask before leaving `channel`, explaining how to return.
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
        "You can rejoin with Browse channels in ⌘K."
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AppContext as _, Bounds, TestAppContext, WindowBounds, WindowOptions, size};
    use mattermost::{
        Client, User,
        testing::{Network, Reply, SlowServer},
    };
    use serde_json::json;
    use std::time::{Duration, Instant};

    struct BoundaryHost {
        sidebar: Entity<SidebarView>,
        after: FocusHandle,
    }

    impl Render for BoundaryHost {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            v_flex()
                .size_full()
                .child(div().flex_1().min_h_0().child(self.sidebar.clone()))
                .child(
                    Button::new("after-sidebar")
                        .label("After sidebar")
                        .track_focus(&self.after),
                )
        }
    }

    struct Fixture {
        _server: SlowServer,
        handle: gpui_kit::AnyWindowHandle,
        view: Entity<SidebarView>,
        session: Entity<Session>,
        after: FocusHandle,
        read_dm: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    fn open_sidebar(cx: &mut TestAppContext) -> Fixture {
        let read_dm = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let remote = read_dm.clone();
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            match (request.method(), request.path().split('?').next().unwrap_or_default()) {
                ("GET", "/api/v4/users/me/teams") => Reply::json(json!([{"id":"team", "name":"team"}])),
                ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!(
                    (0..5000).map(|ix| json!({"id":format!("c-{ix}"), "name":format!("channel-{ix:04}"), "type":"O"})).chain((0..70).map(|ix| json!({"id":format!("dm-{ix}"), "name":format!("me__person-{ix}"), "type":"D", "last_post_at":ix+1}))).collect::<Vec<_>>()
                )),
                ("GET", "/api/v4/users/me/preferences/favorite_channel") => Reply::json(json!([{
                    "user_id":"me", "category":"favorite_channel", "name":"c-0", "value":"true"
                }])),
                ("GET", "/api/v4/users/me/teams/team/channels/members") => Reply::json(json!(
                    (0..70).map(|ix| json!({"channel_id":format!("dm-{ix}"), "user_id":"me", "last_viewed_at":
                        if ix < 20 && !(ix == 19 && remote.load(std::sync::atomic::Ordering::SeqCst)) {0} else {1000}
                    })).collect::<Vec<_>>()
                )),
                ("DELETE", path) if path.starts_with("/api/v4/channels/") => Reply::json(json!({})),
                ("GET", "/api/v4/emoji") => Reply::json(json!([])),
                ("POST", "/api/v4/users/status/ids") => Reply::json(json!([])),
                _ => Reply::not_found(),
            }
        });
        cx.executor().allow_parking();
        cx.update(|cx| {
            gpui_kit::init(cx);
            init(cx);
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let me: User = serde_json::from_value(json!({"id":"me"})).unwrap();
        let session = cx.new(|cx| Session::new(client, me, Some("team".into()), cx));
        let slot = Rc::new(std::cell::RefCell::new(None));
        let after = cx.update(|cx| cx.focus_handle().tab_stop(true));
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(320.), px(700.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let view = cx.new(|cx| SidebarView::new(session.clone(), window, cx));
                    *slot.borrow_mut() = Some(view.clone());
                    cx.new(|_| BoundaryHost {
                        sidebar: view,
                        after: after.clone(),
                    })
                },
            )
            .unwrap()
        });
        cx.update_window(handle, |_, window, _| window.activate_window())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if session.read_with(cx, |s, _| s.is_loaded() && s.is_favorite(&"c-0".into())) {
                break;
            }
            assert!(Instant::now() < deadline);
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
        let view = slot.borrow_mut().take().unwrap();
        Fixture {
            _server: server,
            handle,
            view,
            session,
            after,
            read_dm,
        }
    }

    #[gpui_kit::test]
    fn large_sidebar_virtualizes_and_collapsed_sections_build_no_channels(cx: &mut TestAppContext) {
        let fixture = open_sidebar(cx);
        let Fixture {
            handle,
            ref view,
            ref session,
            ..
        } = fixture;
        let frame_work = |cx: &mut TestAppContext| {
            view.read_with(cx, |v, _| v.channel_rows_built.set(0));
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |_, cx| cx.notify());
                window.render_frame(cx);
            })
            .unwrap();
            view.read_with(cx, |v, _| v.channel_rows_built.get())
        };
        let work = frame_work(cx);
        assert!(
            work > 0 && work < 150,
            "only viewport/overdraw rows: {work}"
        );
        let snapshot = view.read_with(cx, |v, _| v.rows.as_ref().unwrap().sections.clone());
        for _ in 0..3 {
            assert!(frame_work(cx) < 150);
        }
        view.read_with(cx, |v, _| {
            assert!(Rc::ptr_eq(&snapshot, &v.rows.as_ref().unwrap().sections))
        });
        // Native disclosure activation removes every channel from the list model.
        cx.update_window(handle, |_, window, cx| {
            window.click("favorites-section", cx);
            window.click("channels-section", cx);
            window.click("direct-section", cx);
        })
        .unwrap();
        assert_eq!(frame_work(cx), 0);
        view.read_with(cx, |v, _| {
            assert!(
                !v.rows
                    .as_ref()
                    .unwrap()
                    .rows
                    .iter()
                    .any(|r| matches!(r, SidebarRow::Channel(_)))
            )
        });
        cx.update_window(handle, |_, window, cx| {
            window.click("favorites-section", cx);
            window.click("channels-section", cx);
        })
        .unwrap();
        assert!(frame_work(cx) < 150);
        // Tab crosses a virtual viewport boundary and preserves domain focus.
        cx.update_window(handle, |_, window, cx| {
            view.read(cx).row_focus[&SidebarRow::Channel("c-4998".into())]
                .clone()
                .focus(window, cx);
            window.render_frame(cx);
            window.press("tab", cx);
            window.render_frame(cx);
            assert!(
                view.read(cx).row_focus[&SidebarRow::Channel("c-4999".into())].is_focused(window)
            );
            window.press("shift-tab", cx);
            assert!(
                view.read(cx).row_focus[&SidebarRow::Channel("c-4998".into())].is_focused(window)
            );
            window.press("tab", cx);
        })
        .unwrap();
        assert!(frame_work(cx) < 150);
        // The same focused channel survives a favorite reorder.
        session.update(cx, |s, cx| s.set_favorite(&"c-4999".into(), true, cx));
        assert!(frame_work(cx) < 150);
        cx.update_window(handle, |_, window, cx| {
            assert!(
                view.read(cx).row_focus[&SidebarRow::Channel("c-4999".into())].is_focused(window)
            );
        })
        .unwrap();
    }

    fn press(fixture: &Fixture, key: &str, cx: &mut TestAppContext) {
        cx.update_window(fixture.handle, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
        cx.update_window(fixture.handle, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    fn scroll_to_row(
        view: &Entity<SidebarView>,
        row: &SidebarRow,
        offset: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let sidebar = view.read(cx);
        let item_ix = sidebar
            .rows
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .position(|candidate| candidate == row)
            .unwrap();
        sidebar.list.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(offset),
        });
        window.render_frame(cx);
    }

    #[gpui_kit::test]
    fn tab_enters_and_exits_model_boundaries_when_scrolled(cx: &mut TestAppContext) {
        let fixture = open_sidebar(cx);
        cx.update_window(fixture.handle, |_, window, cx| {
            window.render_frame(cx);
            fixture.view.read(cx).row_focus[&SidebarRow::Threads]
                .clone()
                .focus(window, cx);
            scroll_to_row(
                &fixture.view,
                &SidebarRow::Channel("c-2500".into()),
                0.,
                window,
                cx,
            );
            assert_eq!(
                fixture.view.read(cx).list.item_is_above_viewport(0),
                Some(true)
            );
        })
        .unwrap();
        press(&fixture, "shift-tab", cx);
        cx.update_window(fixture.handle, |_, window, _| {
            assert_eq!(
                window.find("jump-to").focused(),
                Some(true),
                "backwards exit must reach the header, not a painted channel"
            );
        })
        .unwrap();
        press(&fixture, "tab", cx);
        cx.update_window(fixture.handle, |_, window, cx| {
            let sidebar = fixture.view.read(cx);
            assert!(sidebar.row_focus[&SidebarRow::Threads].is_focused(window));
            assert_eq!(
                sidebar.list.item_is_above_viewport(0),
                Some(false),
                "forward entry reveals Threads"
            );
        })
        .unwrap();

        let last = fixture.view.read_with(cx, |sidebar, _| {
            sidebar
                .rows
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .rev()
                .find(|row| sidebar.row_focus.contains_key(*row))
                .unwrap()
                .clone()
        });
        cx.update_window(fixture.handle, |_, window, cx| {
            fixture.view.read(cx).row_focus[&last]
                .clone()
                .focus(window, cx);
            scroll_to_row(
                &fixture.view,
                &SidebarRow::Channel("c-2500".into()),
                0.,
                window,
                cx,
            );
        })
        .unwrap();
        press(&fixture, "tab", cx);
        cx.update_window(fixture.handle, |_, window, _| {
            assert!(
                fixture.after.is_focused(window),
                "forward exit must leave the virtual list"
            );
        })
        .unwrap();
        press(&fixture, "shift-tab", cx);
        cx.update_window(fixture.handle, |_, window, cx| {
            let sidebar = fixture.view.read(cx);
            let ix = sidebar
                .rows
                .as_ref()
                .unwrap()
                .rows
                .iter()
                .position(|row| row == &last)
                .unwrap();
            assert!(sidebar.row_focus[&last].is_focused(window));
            assert_eq!(
                sidebar.list.item_is_below_viewport(ix),
                Some(false),
                "backwards entry reveals the last model row"
            );
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn removing_the_top_channel_preserves_its_neighbor_and_offset(cx: &mut TestAppContext) {
        let fixture = open_sidebar(cx);
        cx.update_window(fixture.handle, |_, window, cx| {
            window.render_frame(cx);
            scroll_to_row(
                &fixture.view,
                &SidebarRow::Channel("c-2500".into()),
                12.,
                window,
                cx,
            );
            fixture
                .session
                .update(cx, |s, cx| s.leave_channel(&"c-2500".into(), cx));
            window.render_frame(cx);
            let sidebar = fixture.view.read(cx);
            let top = sidebar.list.logical_scroll_top();
            assert_eq!(
                sidebar.rows.as_ref().unwrap().rows[top.item_ix],
                SidebarRow::Channel("c-2501".into())
            );
            assert_eq!(top.offset_in_item, px(12.));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn reading_an_older_dm_preserves_the_neighbor_in_the_viewport(cx: &mut TestAppContext) {
        let fixture = open_sidebar(cx);
        cx.update_window(fixture.handle, |_, window, cx| {
            window.render_frame(cx);
            scroll_to_row(
                &fixture.view,
                &SidebarRow::Channel("dm-19".into()),
                12.,
                window,
                cx,
            );
            let sidebar = fixture.view.read(cx);
            assert_eq!(
                sidebar.rows.as_ref().unwrap().rows[sidebar.list.logical_scroll_top().item_ix],
                SidebarRow::Channel("dm-19".into())
            );
        })
        .unwrap();
        // An authoritative reload observes a read made by another client.
        fixture
            .read_dm
            .store(true, std::sync::atomic::Ordering::SeqCst);
        fixture.session.update(cx, |s, cx| s.retry_channels(cx));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if fixture.session.read_with(cx, |s, _| {
                s.is_loaded() && !s.channel(&"dm-19".into()).unwrap().is_unread()
            }) {
                break;
            }
            assert!(Instant::now() < deadline);
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
        cx.update_window(fixture.handle, |_, window, cx| {
            window.render_frame(cx);
            let sidebar = fixture.view.read(cx);
            let top = sidebar.list.logical_scroll_top();
            assert_eq!(
                sidebar.rows.as_ref().unwrap().rows[top.item_ix],
                SidebarRow::Channel("dm-18".into())
            );
            assert_eq!(top.offset_in_item, px(12.));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn collapsing_the_anchor_section_falls_back_to_its_header(cx: &mut TestAppContext) {
        let fixture = open_sidebar(cx);
        cx.update_window(fixture.handle, |_, window, cx| {
            window.render_frame(cx);
            scroll_to_row(
                &fixture.view,
                &SidebarRow::Channel("c-1".into()),
                30.,
                window,
                cx,
            );
            fixture.view.update(cx, |sidebar, cx| {
                sidebar.channels_collapsed = true;
                cx.notify();
            });
            window.render_frame(cx);
            let sidebar = fixture.view.read(cx);
            let top = sidebar.list.logical_scroll_top();
            assert_eq!(
                sidebar.rows.as_ref().unwrap().rows[top.item_ix],
                SidebarRow::Header(Section::Channels)
            );
            assert_eq!(
                top.offset_in_item,
                px(0.),
                "an unmeasured replacement starts at a safe clamped offset"
            );
        })
        .unwrap();
    }
}
