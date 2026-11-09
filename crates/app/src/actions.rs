//! Application commands, their key bindings, and the menu bar.
//!
//! Each command is one GPUI Action, so the menu bar, key bindings, the
//! command palette, and buttons all dispatch the same thing.

use gpui_kit::component::kbd::Kbd;
use gpui_kit::{App, KeyBinding, Keystroke, Menu, MenuItem, actions};

use crate::{
    appearance,
    persist::Appearance,
    update::{self, UpdateStatus},
};

actions!(
    mtc,
    [
        Quit,
        /// Jump to any channel or person by name.
        QuickSwitch,
        /// Search the messages of the open conversation.
        SearchConversation,
        /// Show the threads you follow.
        ShowThreads,
        CloseConversation,
        /// Add the open conversation to favorites, or remove it.
        ToggleFavorite,
        /// Leave the open channel, after asking.
        LeaveChannel,
        NextChannel,
        PreviousChannel,
        NextUnreadChannel,
        PreviousUnreadChannel,
        /// Close the thread panel, or mark the channel read.
        Dismiss,
        /// Move keyboard focus to the message field.
        FocusComposer,
        ZoomIn,
        ZoomOut,
        ResetZoom,
        UseSystemAppearance,
        UseLightAppearance,
        UseDarkAppearance,
        ShowShortcuts,
        /// Appearance, zoom, updates, and the version of this build.
        ShowSettings,
        SignOut,
        CheckForUpdates,
        /// Quit and open the update that was installed in the background.
        RestartToUpdate,
        /// Turn checking for and installing updates on a schedule on or off.
        ToggleAutomaticUpdates,
    ]
);

/// A keystroke with the platform's primary modifier: ⌘ on macOS, Ctrl on
/// Linux and Windows. GPUI reads a literal `cmd-` as the Super/Windows key
/// there, which the desktop usually claims for itself.
#[cfg(target_os = "macos")]
macro_rules! primary {
    ($key:literal) => {
        concat!("cmd-", $key)
    };
}
#[cfg(not(target_os = "macos"))]
macro_rules! primary {
    ($key:literal) => {
        concat!("ctrl-", $key)
    };
}

/// The quick switcher's shortcut, shown in buttons that open it.
pub const QUICK_SWITCH_KEY: &str = primary!("k");

pub const ZOOM_IN_KEY: &str = primary!("=");
pub const ZOOM_OUT_KEY: &str = primary!("-");
pub const RESET_ZOOM_KEY: &str = primary!("0");

/// How the platform writes a shortcut such as [`QUICK_SWITCH_KEY`]:
/// `⌘K` on macOS, `Ctrl+K` elsewhere.
pub fn shortcut_label(keystroke: &'static str) -> String {
    Keystroke::parse(keystroke)
        .map(|keystroke| Kbd::format(&keystroke))
        .unwrap_or_else(|_| keystroke.to_string())
}

/// The key context of the signed-in workspace.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

pub fn init(cx: &mut App) {
    // Bind before building the menu bar: the menus read the keymap once.
    cx.bind_keys([
        KeyBinding::new(primary!("q"), Quit, None),
        KeyBinding::new(QUICK_SWITCH_KEY, QuickSwitch, None),
        KeyBinding::new(primary!("t"), QuickSwitch, None),
        KeyBinding::new(primary!("f"), SearchConversation, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(primary!("shift-t"), ShowThreads, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("alt-down", NextChannel, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("alt-up", PreviousChannel, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("alt-shift-down", NextUnreadChannel, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(
            "alt-shift-up",
            PreviousUnreadChannel,
            Some(WORKSPACE_CONTEXT),
        ),
        KeyBinding::new("escape", Dismiss, Some(WORKSPACE_CONTEXT)),
        // The message field binds these itself; in the workspace the channel
        // commands are more useful, so they win inside its inputs.
        KeyBinding::new("escape", Dismiss, Some("Workspace > Input")),
        KeyBinding::new(
            "alt-shift-down",
            NextUnreadChannel,
            Some("Workspace > Input"),
        ),
        KeyBinding::new(
            "alt-shift-up",
            PreviousUnreadChannel,
            Some("Workspace > Input"),
        ),
        KeyBinding::new(primary!("f"), SearchConversation, Some("Workspace > Input")),
        KeyBinding::new(primary!("w"), CloseConversation, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(primary!("l"), FocusComposer, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new(ZOOM_IN_KEY, ZoomIn, None),
        KeyBinding::new(primary!("+"), ZoomIn, None),
        KeyBinding::new(ZOOM_OUT_KEY, ZoomOut, None),
        KeyBinding::new(RESET_ZOOM_KEY, ResetZoom, None),
        KeyBinding::new(primary!("/"), ShowShortcuts, None),
        KeyBinding::new(primary!(","), ShowSettings, None),
    ]);

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &ZoomIn, cx| appearance::zoom(1., cx));
    cx.on_action(|_: &ZoomOut, cx| appearance::zoom(-1., cx));
    cx.on_action(|_: &ResetZoom, cx| appearance::reset_zoom(cx));
    cx.on_action(|_: &UseSystemAppearance, cx| appearance::set(Appearance::System, cx));
    cx.on_action(|_: &UseLightAppearance, cx| appearance::set(Appearance::Light, cx));
    cx.on_action(|_: &UseDarkAppearance, cx| appearance::set(Appearance::Dark, cx));
    cx.on_action(|_: &CheckForUpdates, cx| update::check(cx));
    cx.on_action(|_: &RestartToUpdate, cx| update::restart(cx));
    cx.on_action(|_: &ToggleAutomaticUpdates, cx| {
        update::set_automatic(!update::is_automatic(), cx)
    });

    set_menus(cx);
}

/// Build the menu bar. Call again when an item's state changes.
pub fn set_menus(cx: &mut App) {
    let update = match update::status(cx) {
        UpdateStatus::Ready { .. } => MenuItem::action("Restart to Update", RestartToUpdate),
        _ => MenuItem::action("Check for Updates", CheckForUpdates),
    };
    cx.set_menus([
        Menu::new("mtc").items([
            MenuItem::action("Settings…", ShowSettings),
            MenuItem::action("Keyboard Shortcuts…", ShowShortcuts),
            MenuItem::separator(),
            update,
            MenuItem::action("Update Automatically", ToggleAutomaticUpdates)
                .checked(update::is_automatic()),
            MenuItem::separator(),
            MenuItem::action("Sign Out", SignOut),
            MenuItem::separator(),
            MenuItem::action("Quit mtc", Quit),
        ]),
        Menu::new("View").items([
            MenuItem::action("Zoom In", ZoomIn),
            MenuItem::action("Zoom Out", ZoomOut),
            MenuItem::action("Actual Size", ResetZoom),
            MenuItem::separator(),
            MenuItem::action("Match System Appearance", UseSystemAppearance),
            MenuItem::action("Light Appearance", UseLightAppearance),
            MenuItem::action("Dark Appearance", UseDarkAppearance),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Jump To…", QuickSwitch),
            MenuItem::action("Search Messages…", SearchConversation),
            MenuItem::action("Threads", ShowThreads),
            MenuItem::action("Close Conversation", CloseConversation),
            MenuItem::separator(),
            MenuItem::action("Next Channel", NextChannel),
            MenuItem::action("Previous Channel", PreviousChannel),
            MenuItem::action("Next Unread Channel", NextUnreadChannel),
            MenuItem::action("Previous Unread Channel", PreviousUnreadChannel),
            MenuItem::separator(),
            MenuItem::action("Message Field", FocusComposer),
        ]),
    ]);
}
