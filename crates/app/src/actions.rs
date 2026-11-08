//! Application commands, their key bindings, and the menu bar.
//!
//! Each command is one GPUI Action, so the menu bar, key bindings, the
//! command palette, and buttons all dispatch the same thing.

use gpui_kit::{App, KeyBinding, Menu, MenuItem, actions};

use crate::{appearance, persist::Appearance};

actions!(
    mtc,
    [
        Quit,
        /// Jump to any channel or person by name.
        QuickSwitch,
        CloseConversation,
        /// Add the open conversation to favorites, or remove it.
        ToggleFavorite,
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
        SignOut,
    ]
);

/// The key context of the signed-in workspace.
pub const WORKSPACE_CONTEXT: &str = "Workspace";

pub fn init(cx: &mut App) {
    // Bind before building the menu bar: the menus read the keymap once.
    cx.bind_keys([
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-k", QuickSwitch, None),
        KeyBinding::new("cmd-t", QuickSwitch, None),
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
        KeyBinding::new("cmd-w", CloseConversation, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-l", FocusComposer, Some(WORKSPACE_CONTEXT)),
        KeyBinding::new("cmd-=", ZoomIn, None),
        KeyBinding::new("cmd-+", ZoomIn, None),
        KeyBinding::new("cmd--", ZoomOut, None),
        KeyBinding::new("cmd-0", ResetZoom, None),
        KeyBinding::new("cmd-/", ShowShortcuts, None),
    ]);

    cx.on_action(|_: &Quit, cx| cx.quit());
    cx.on_action(|_: &ZoomIn, cx| appearance::zoom(1., cx));
    cx.on_action(|_: &ZoomOut, cx| appearance::zoom(-1., cx));
    cx.on_action(|_: &ResetZoom, cx| appearance::reset_zoom(cx));
    cx.on_action(|_: &UseSystemAppearance, cx| appearance::set(Appearance::System, cx));
    cx.on_action(|_: &UseLightAppearance, cx| appearance::set(Appearance::Light, cx));
    cx.on_action(|_: &UseDarkAppearance, cx| appearance::set(Appearance::Dark, cx));

    set_menus(cx);
}

pub fn set_menus(cx: &mut App) {
    cx.set_menus([
        Menu::new("mtc").items([
            MenuItem::action("Keyboard Shortcuts…", ShowShortcuts),
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
