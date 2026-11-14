//! ⌘K: jump to any channel or person, or run an app command, by typing.

use std::{cell::Cell, collections::HashSet, rc::Rc};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IndexPath, Sizable as _, WindowExt as _,
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    kbd::Kbd,
};
use gpui_kit::{
    Action, App, AppContext as _, Entity, Focusable as _, ParentElement as _, SharedString,
    Styled as _, WeakEntity, Window, div,
};
use mattermost::{ChannelId, ChannelType};
use session::Session;

use crate::{actions, workspace::Workspace};

enum Target {
    Channel(ChannelId),
    Action(Box<dyn Action>),
}

struct Group {
    label: &'static str,
    items: Vec<(CommandItem, Target)>,
}

fn channel_item(session: &Session, id: &ChannelId, cx: &App) -> Option<(CommandItem, Target)> {
    let entry = session.channel(id)?;
    let label = session.channel_label(entry, cx);
    let icon = match entry.kind() {
        ChannelType::Open => IconName::Hash,
        ChannelType::Private => IconName::Lock,
        ChannelType::Direct => IconName::AtSign,
        ChannelType::Group => IconName::Users,
    };
    let channel = entry.channel();
    let mut keywords = vec![SharedString::from(channel.name.clone())];
    // A group's server name lists its members' usernames.
    if entry.kind() == ChannelType::Group {
        keywords.push(channel.display_name.clone().into());
    }
    // Initials let "ts" find "Town Square".
    let initials: String = label
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|w| w.chars().next())
        .collect();
    if initials.len() > 1 {
        keywords.push(initials.into());
    }
    if let Some(partner) = channel.direct_partner(&session.me().id)
        && let Some(user) = session.directory().read(cx).user(&partner)
    {
        keywords.push(user.username.clone().into());
    }
    let item = CommandItem::new()
        .label(label)
        .icon(icon)
        .keywords(keywords);
    Some((item, Target::Channel(id.clone())))
}

fn action_item(label: &'static str, icon: IconName, action: impl Action) -> (CommandItem, Target) {
    let action: Box<dyn Action> = Box::new(action);
    let hint = action.boxed_clone();
    let item = CommandItem::new().label(label).child(move |window, cx| {
        h_flex()
            .w_full()
            .gap_2()
            .child(
                Icon::new(icon)
                    .small()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().flex_1().child(label))
            .children(Kbd::binding_for_action(
                &*hint,
                Some(actions::WORKSPACE_CONTEXT),
                window,
            ))
    });
    (item, Target::Action(action))
}

/// Open the switcher over `window`.
pub fn open(
    workspace: WeakEntity<Workspace>,
    session: &Entity<Session>,
    recent: &[ChannelId],
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let session_ref = session.read(cx);
    let sections = crate::sidebar::conversation_sections(session_ref, cx);
    let active = session_ref.active_channel().cloned();

    // The open conversation is left out of Recent but stays findable below.
    let mut seen = HashSet::new();
    let recent_ids: Vec<&ChannelId> = recent
        .iter()
        .filter(|id| Some(*id) != active.as_ref() && seen.insert(*id))
        .take(5)
        .collect();
    let recent_items: Vec<_> = recent_ids
        .iter()
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let in_recent = |id: &ChannelId| recent_ids.contains(&id);
    let favorite_items: Vec<_> = sections
        .favorites
        .iter()
        .filter(|id| !in_recent(id))
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let channel_items: Vec<_> = sections
        .channels
        .iter()
        .filter(|id| !in_recent(id))
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let direct_items: Vec<_> = sections
        .direct
        .iter()
        .filter(|id| !in_recent(id))
        .filter_map(|id| channel_item(session_ref, id, cx))
        .collect();
    let mut commands = Vec::new();
    if let Some(active) = &active {
        let favorite = session_ref.is_favorite(active);
        commands.push(action_item(
            crate::sidebar::favorite_label(favorite),
            if favorite {
                IconName::StarOff
            } else {
                IconName::Star
            },
            actions::ToggleFavorite,
        ));
        if !session_ref.is_mute_pending(active)
            && let Some(channel) = session_ref.channel(active)
        {
            let muted = channel.is_muted();
            commands.push(action_item(
                crate::sidebar::mute_label(muted),
                if muted {
                    IconName::Bell
                } else {
                    IconName::BellOff
                },
                actions::ToggleChannelMute,
            ));
        }
        if session_ref.channel(active).is_some_and(|e| e.can_leave()) {
            commands.push(action_item(
                "Leave channel",
                IconName::LogOut,
                actions::LeaveChannel,
            ));
        }
    }
    if active.is_some() {
        commands.push(action_item(
            "Search messages",
            IconName::Search,
            actions::SearchConversation,
        ));
    }
    commands.extend([
        action_item("Threads", IconName::MessagesSquare, actions::ShowThreads),
        action_item(
            "Close conversation",
            IconName::X,
            actions::CloseConversation,
        ),
        action_item(
            "Next unread channel",
            IconName::ArrowDown,
            actions::NextUnreadChannel,
        ),
        action_item("Zoom in", IconName::ZoomIn, actions::ZoomIn),
        action_item("Zoom out", IconName::ZoomOut, actions::ZoomOut),
        action_item("Actual size", IconName::Scan, actions::ResetZoom),
        action_item(
            "Light appearance",
            IconName::Sun,
            actions::UseLightAppearance,
        ),
        action_item(
            "Dark appearance",
            IconName::Moon,
            actions::UseDarkAppearance,
        ),
        action_item(
            "Match system appearance",
            IconName::SunMoon,
            actions::UseSystemAppearance,
        ),
        action_item("Settings", IconName::Settings, actions::ShowSettings),
        action_item(
            "Keyboard shortcuts",
            IconName::Keyboard,
            actions::ShowShortcuts,
        ),
    ]);
    commands.push(match crate::update::status(cx) {
        crate::update::UpdateStatus::Ready { .. } => action_item(
            "Restart to update",
            IconName::CircleArrowUp,
            actions::RestartToUpdate,
        ),
        _ => action_item(
            "Check for updates",
            IconName::RefreshCw,
            actions::CheckForUpdates,
        ),
    });
    commands.push(action_item(
        if crate::update::is_automatic() {
            "Turn off automatic updates"
        } else {
            "Turn on automatic updates"
        },
        IconName::Settings,
        actions::ToggleAutomaticUpdates,
    ));
    commands.push(action_item("Sign out", IconName::LogOut, actions::SignOut));

    let groups: Vec<Group> = [
        Group {
            label: "Recent",
            items: recent_items,
        },
        Group {
            label: "Favorites",
            items: favorite_items,
        },
        Group {
            label: "Channels",
            items: channel_items,
        },
        Group {
            label: "Direct messages",
            items: direct_items,
        },
        Group {
            label: "Commands",
            items: commands,
        },
    ]
    .into_iter()
    .filter(|g| !g.items.is_empty())
    .collect();

    let command_groups: Vec<CommandGroup> = groups
        .iter()
        .map(|g| {
            CommandGroup::new()
                .label(g.label)
                .items(g.items.iter().map(|(item, _)| item.clone()))
        })
        .collect();
    let targets: Rc<Vec<Vec<Target>>> = Rc::new(
        groups
            .into_iter()
            .map(|g| g.items.into_iter().map(|(_, t)| t).collect())
            .collect(),
    );

    let state = cx.new(|cx| CommandState::new(window, cx));
    let initial_focus = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if initial_focus.replace(false) {
            state.focus_handle(cx).focus(window, cx);
        }
        let state = state.clone();
        let command_groups = command_groups.clone();
        let targets = targets.clone();
        let workspace = workspace.clone();
        dialog
            .close_button(false)
            .p_2()
            // Dialog accepts resolved geometry, so project its width from the current rem.
            .w(window.rem_size() * 44.)
            .content(move |content, _, _| {
                let targets = targets.clone();
                let workspace = workspace.clone();
                let mut command = Command::new(&state)
                    .bordered(false)
                    .max_h(gpui_kit::rems(32.))
                    .placeholder("Find a conversation or command…");
                for group in command_groups.iter().cloned() {
                    command = command.group(group);
                }
                content.child(command.on_confirm(move |path: IndexPath, window, cx| {
                    let Some(target) = targets.get(path.section).and_then(|g| g.get(path.row))
                    else {
                        return;
                    };
                    window.close_dialog(cx);
                    match target {
                        Target::Channel(id) => {
                            let id = id.clone();
                            let _ = workspace.update(cx, |w, cx| w.open_channel(&id, window, cx));
                        }
                        Target::Action(action) => {
                            // After the dialog closes, so the action reaches
                            // the workspace through the restored focus.
                            let action = action.boxed_clone();
                            window.defer(cx, move |window, cx| window.dispatch_action(action, cx));
                        }
                    }
                }))
            })
    });
}
