//! The extension seam.
//!
//! An [`Extension`] can add slash commands, render particular posts its own
//! way, and observe session events (to notify, log, or integrate). The
//! built-in behavior is itself registered as an extension, so the seam is
//! exercised by the app every day rather than bolted on.
//!
//! ```ignore
//! struct Echo;
//! impl Extension for Echo {
//!     fn name(&self) -> &'static str { "echo" }
//!     fn slash_commands(&self) -> Vec<SlashCommand> {
//!         vec![SlashCommand::new("loud", "Send in capitals")
//!             .hint("<message>")
//!             .on_run(|args, _, _, _| CommandOutcome::Send(args.to_uppercase()))]
//!     }
//! }
//! extensions::register(Echo, cx);
//! ```

use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, WindowExt as _, notification::Notification};
use gpui_kit::{
    AnyElement, App, Entity, Global, IntoElement as _, ParentElement as _, SharedString,
    Styled as _, Window, div,
};
use session::{Entry, Session, SessionEvent, TimelineKind};

use crate::{actions::ShowShortcuts, persist::Appearance};

/// What running a slash command produced.
pub enum CommandOutcome {
    /// Send this text as a post.
    Send(String),
    /// The command did its work; nothing is sent.
    Handled,
    /// Not handled here; let the server run it.
    Pass,
}

/// Where a slash command runs.
pub struct CommandContext {
    session: Entity<Session>,
    kind: TimelineKind,
}

impl CommandContext {
    pub fn new(session: Entity<Session>, kind: TimelineKind) -> Self {
        Self { session, kind }
    }

    pub fn session(&self) -> &Entity<Session> {
        &self.session
    }

    pub fn kind(&self) -> &TimelineKind {
        &self.kind
    }
}

type RunCommand = dyn Fn(&str, &CommandContext, &mut Window, &mut App) -> CommandOutcome;

/// A `/command` typed in the message field.
#[derive(Clone)]
pub struct SlashCommand {
    name: SharedString,
    description: SharedString,
    hint: SharedString,
    run: Rc<RunCommand>,
}

impl SlashCommand {
    /// `name` is the word after the slash.
    pub fn new(name: impl Into<SharedString>, description: impl Into<SharedString>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            hint: SharedString::default(),
            run: Rc::new(|_, _, _, _| CommandOutcome::Pass),
        }
    }

    /// The argument shape, such as `<message>`.
    pub fn hint(mut self, hint: impl Into<SharedString>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn on_run(
        mut self,
        run: impl Fn(&str, &CommandContext, &mut Window, &mut App) -> CommandOutcome + 'static,
    ) -> Self {
        self.run = Rc::new(run);
        self
    }

    pub fn name(&self) -> &SharedString {
        &self.name
    }

    pub fn description(&self) -> &SharedString {
        &self.description
    }

    pub fn hint_text(&self) -> &SharedString {
        &self.hint
    }
}

/// A unit of optional behavior. Every method has a default, so an extension
/// implements only what it adds.
pub trait Extension: 'static {
    fn name(&self) -> &'static str;

    fn slash_commands(&self) -> Vec<SlashCommand> {
        Vec::new()
    }

    /// Render a post's body instead of the default text. The first extension
    /// that returns `Some` wins. Called during render: keep it pure and cheap.
    fn render_post_body(&self, _entry: &Entry, _cx: &App) -> Option<AnyElement> {
        None
    }

    /// Observe a session event.
    fn on_session_event(
        &self,
        _event: &SessionEvent,
        _session: &Entity<Session>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }
}

#[derive(Default)]
struct Extensions {
    extensions: Vec<Rc<dyn Extension>>,
    commands: Vec<SlashCommand>,
}

impl Global for Extensions {}

/// Add an extension, replacing one registered under the same name. Later
/// registrations take precedence for slash commands with the same name.
pub fn register(extension: impl Extension, cx: &mut App) {
    let extension: Rc<dyn Extension> = Rc::new(extension);
    let registry = cx.default_global::<Extensions>();
    let new_commands = extension.slash_commands();
    registry
        .commands
        .retain(|c| !new_commands.iter().any(|n| n.name == c.name));
    registry.commands.extend(new_commands);
    registry.commands.sort_by(|a, b| a.name.cmp(&b.name));
    log::debug!("registered extension {}", extension.name());
    registry.extensions.retain(|e| e.name() != extension.name());
    registry.extensions.push(extension);
}

pub fn slash_commands(cx: &App) -> &[SlashCommand] {
    cx.try_global::<Extensions>()
        .map(|r| r.commands.as_slice())
        .unwrap_or_default()
}

/// Run `text` if it is a known slash command.
pub fn run_slash_command(
    text: &str,
    context: &CommandContext,
    window: &mut Window,
    cx: &mut App,
) -> CommandOutcome {
    let Some(rest) = text.strip_prefix('/') else {
        return CommandOutcome::Pass;
    };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let command = slash_commands(cx)
        .iter()
        .find(|c| c.name.as_ref() == name)
        .cloned();
    match command {
        Some(command) => (command.run)(args.trim(), context, window, cx),
        None => CommandOutcome::Pass,
    }
}

pub fn render_post_body(entry: &Entry, cx: &App) -> Option<AnyElement> {
    let registry = cx.try_global::<Extensions>()?;
    registry
        .extensions
        .iter()
        .find_map(|e| e.render_post_body(entry, cx))
}

pub fn dispatch_session_event(
    event: &SessionEvent,
    session: &Entity<Session>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(registry) = cx.try_global::<Extensions>() else {
        return;
    };
    let extensions = registry.extensions.clone();
    for extension in extensions {
        extension.on_session_event(event, session, window, cx);
    }
}

pub fn init(cx: &mut App) {
    register(Core, cx);
}

/// The built-in commands, system-message rendering, and notifications.
struct Core;

impl Extension for Core {
    fn name(&self) -> &'static str {
        "core"
    }

    fn slash_commands(&self) -> Vec<SlashCommand> {
        vec![
            SlashCommand::new("shrug", "Add ¯\\_(ツ)_/¯ to your message")
                .hint("[message]")
                .on_run(|args, _, _, _| CommandOutcome::Send(join(args, r"¯\\\_(ツ)\_/¯"))),
            SlashCommand::new("tableflip", "Add (╯°□°)╯︵ ┻━┻ to your message")
                .hint("[message]")
                .on_run(|args, _, _, _| CommandOutcome::Send(join(args, "(╯°□°)╯︵ ┻━┻"))),
            SlashCommand::new("unflip", "Add ┬─┬ ノ( ゜-゜ノ) to your message")
                .hint("[message]")
                .on_run(|args, _, _, _| CommandOutcome::Send(join(args, "┬─┬ ノ( ゜-゜ノ)"))),
            SlashCommand::new("me", "Describe an action")
                .hint("<action>")
                .on_run(|args, _, _, _| {
                    if args.is_empty() {
                        CommandOutcome::Handled
                    } else {
                        CommandOutcome::Send(format!("*{args}*"))
                    }
                }),
            SlashCommand::new("theme", "Switch appearance")
                .hint("light | dark | system")
                .on_run(|args, _, _, cx| {
                    let appearance = match args {
                        "light" => Appearance::Light,
                        "dark" => Appearance::Dark,
                        _ => Appearance::System,
                    };
                    crate::appearance::set(appearance, cx);
                    CommandOutcome::Handled
                }),
            SlashCommand::new("read", "Mark this channel read").on_run(|_, context, _, cx| {
                let channel = context.kind().channel().clone();
                context
                    .session()
                    .update(cx, |session, cx| session.mark_viewed(&channel, cx));
                CommandOutcome::Handled
            }),
            SlashCommand::new("shortcuts", "Show keyboard shortcuts").on_run(|_, _, window, cx| {
                window.dispatch_action(Box::new(ShowShortcuts), cx);
                CommandOutcome::Handled
            }),
        ]
    }

    fn render_post_body(&self, entry: &Entry, cx: &App) -> Option<AnyElement> {
        let post = entry.post();
        if !post.is_system() {
            return None;
        }
        Some(
            div()
                .text_sm()
                .italic()
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(entry.message().text().to_string()))
                .into_any_element(),
        )
    }

    fn on_session_event(
        &self,
        event: &SessionEvent,
        session: &Entity<Session>,
        window: &mut Window,
        cx: &mut App,
    ) {
        match event {
            SessionEvent::Posted { post, mentions_me } => {
                let session = session.read(cx);
                let Some(channel) = session.channel(&post.channel_id) else {
                    return;
                };
                let is_direct = channel.kind() == mattermost::ChannelType::Direct;
                let is_visible = window.is_window_active()
                    && if post.is_reply() {
                        session.open_thread() == Some(&post.root_id)
                    } else {
                        session.active_channel() == Some(&post.channel_id)
                    };
                let mentions_me = desktop_mentions(
                    post,
                    channel.kind(),
                    channel.desktop_notifications(),
                    *mentions_me,
                    session.me(),
                    channel.ignore_channel_mentions(),
                );
                let status = session.directory().read(cx).status(&session.me().id);
                if post.is_system()
                    || !should_notify(
                        channel.desktop_notifications(),
                        session
                            .me()
                            .notify_props
                            .get("desktop")
                            .map(String::as_str)
                            .unwrap_or("mention"),
                        mentions_me,
                        is_direct,
                        channel.is_muted(),
                        is_visible,
                        status,
                    )
                {
                    return;
                }
                let author = session.directory().read(cx).display_name(&post.user_id);
                let title = if is_direct {
                    author.to_string()
                } else {
                    format!("{author} in {}", session.channel_label(channel, cx))
                };
                let preview: String = post.message.chars().take(160).collect();
                window.push_notification(
                    Notification::new()
                        .title(title)
                        .message(preview)
                        .in_app_and_system(),
                    cx,
                );
            }
            SessionEvent::SendFailed { message } => {
                window.push_notification(Notification::error(message.clone()), cx);
            }
            _ => {}
        }
    }
}

fn join(args: &str, suffix: &str) -> String {
    if args.is_empty() {
        suffix.to_string()
    } else {
        format!("{args} {suffix}")
    }
}

fn desktop_level<'a>(channel: &'a str, account: &'a str) -> &'a str {
    match channel {
        "all" | "mention" | "none" => channel,
        _ => account,
    }
}

/// GMs include every member in the server's mentions list. An explicit channel
/// mentions-only override opts out of those implicit mentions; inherited account
/// settings retain Mattermost's usual GM behavior.
fn desktop_mentions(
    post: &mattermost::Post,
    kind: mattermost::ChannelType,
    desktop: &str,
    mentions_me: bool,
    me: &mattermost::User,
    ignore_channel_mentions: Option<&str>,
) -> bool {
    if kind != mattermost::ChannelType::Group || desktop != "mention" {
        return mentions_me;
    }
    let channel_mentions = match ignore_channel_mentions {
        Some("on" | "true") => false,
        Some("off" | "false") => true,
        _ => me
            .notify_props
            .get("channel")
            .is_none_or(|value| value != "false"),
    };
    if !mentions_me {
        return false;
    }
    if explicit_mention(&post.message, me, channel_mentions) {
        return true;
    }
    let Some(attachments) = post
        .props
        .get("attachments")
        .and_then(serde_json::Value::as_array)
    else {
        return false;
    };
    for attachment in attachments {
        for key in ["text", "pretext", "title"] {
            if let Some(text) = attachment.get(key).and_then(serde_json::Value::as_str)
                && explicit_mention(text, me, channel_mentions)
            {
                return true;
            }
        }
        if let Some(fields) = attachment
            .get("fields")
            .and_then(serde_json::Value::as_array)
        {
            for field in fields {
                for key in ["title", "value"] {
                    if let Some(text) = field.get(key).and_then(serde_json::Value::as_str)
                        && explicit_mention(text, me, channel_mentions)
                    {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn explicit_mention(message: &str, me: &mattermost::User, channel_mentions: bool) -> bool {
    let lower = message.to_lowercase();
    if !me.username.is_empty()
        && contains_mention_key(&lower, &format!("@{}", me.username.to_lowercase()))
    {
        return true;
    }
    if channel_mentions
        && ["@channel", "@all", "@here"]
            .iter()
            .any(|key| contains_mention_key(&lower, key))
    {
        return true;
    }
    if me
        .notify_props
        .get("first_name")
        .is_some_and(|value| value == "true")
        && !me.first_name.is_empty()
        && contains_mention_key(message, &me.first_name)
    {
        return true;
    }
    me.notify_props.get("mention_keys").is_some_and(|keys| {
        keys.split(',')
            .map(str::trim)
            .filter(|key| {
                !key.is_empty()
                    && (channel_mentions
                        || !["@channel", "@all", "@here"].contains(&key.to_lowercase().as_str()))
            })
            .any(|key| contains_mention_key(&lower, &key.to_lowercase()))
    })
}

/// Match whole keys, avoiding partial usernames and addresses. This is a small
/// client-side check used only to disambiguate implicit GM mentions, not a
/// replacement for the server's mention parser.
fn contains_mention_key(message: &str, key: &str) -> bool {
    let word = |c: char| c.is_alphanumeric() || matches!(c, '_' | '-' | '@');
    message.match_indices(key).any(|(ix, _)| {
        let mut suffix = message[ix + key.len()..].chars();
        let next = suffix.next();
        let dotted_name = next == Some('.') && suffix.next().is_some_and(word);
        !message[..ix].chars().next_back().is_some_and(word)
            && !next.is_some_and(word)
            && !dotted_name
    })
}

/// Preferences only reduce the mentions-and-DMs baseline. `all` expansion is
/// deferred; `default` inherits the account setting. Mute and DND always win.
fn should_notify(
    desktop: &str,
    account_desktop: &str,
    mentions_me: bool,
    is_direct: bool,
    muted: bool,
    visible: bool,
    status: mattermost::Status,
) -> bool {
    if muted || visible || status == mattermost::Status::DoNotDisturb {
        return false;
    }
    match desktop_level(desktop, account_desktop) {
        "none" => false,
        _ => mentions_me || is_direct,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mattermost::Status;

    #[test]
    fn desktop_preferences_inherit_and_override_account_defaults() {
        for (channel, account, mention, direct, expected) in [
            ("all", "none", false, false, false),
            ("none", "all", true, true, false),
            ("mention", "all", false, false, false),
            ("mention", "none", true, false, true),
            ("mention", "none", false, true, true),
            ("default", "all", false, false, false),
            ("default", "none", true, true, false),
            ("default", "mention", false, false, false),
            ("default", "mention", true, false, true),
            ("default", "mention", false, true, true),
            ("", "", false, false, false),
            ("default", "default", false, true, true),
        ] {
            assert_eq!(
                should_notify(
                    channel,
                    account,
                    mention,
                    direct,
                    false,
                    false,
                    Status::Online
                ),
                expected,
                "channel={channel}, account={account}, mention={mention}, direct={direct}"
            );
        }
    }

    #[test]
    fn group_mentions_only_override_requires_an_explicit_key() {
        use mattermost::{ChannelType, Post, User};
        let mut me = User::default();
        me.username = "bob".into();
        me.first_name = "Robert".into();
        me.notify_props.insert("first_name".into(), "true".into());
        me.notify_props
            .insert("mention_keys".into(), "launch,release day".into());
        for message in [
            "hi @bob!",
            "hi @bob.",
            "@channel hello",
            "@all",
            "@here",
            "Robert?",
            "LAUNCH",
            "release day!",
        ] {
            let mut post = Post::default();
            post.message = message.into();
            assert!(
                desktop_mentions(&post, ChannelType::Group, "mention", true, &me, None),
                "{message}"
            );
            assert!(!desktop_mentions(
                &post,
                ChannelType::Group,
                "mention",
                false,
                &me,
                None
            ));
        }
        for message in [
            "hello",
            "robert",
            "@bobby",
            "@bob-other",
            "@bob.jones",
            "foo@bob",
            "relaunch",
            "launching",
        ] {
            let mut post = Post::default();
            post.message = message.into();
            assert!(
                !desktop_mentions(&post, ChannelType::Group, "mention", true, &me, None),
                "{message}"
            );
            assert!(desktop_mentions(
                &post,
                ChannelType::Group,
                "default",
                true,
                &me,
                None
            ));
        }
        me.notify_props.insert("channel".into(), "false".into());
        assert!(!explicit_mention("@channel @all @here", &me, false));
        me.notify_props.insert("first_name".into(), "false".into());
        assert!(!explicit_mention("Robert", &me, true));
    }

    #[test]
    fn explicit_group_mentions_include_attachments_and_member_ignore_overrides() {
        use mattermost::{ChannelType, Post, User};
        use serde_json::json;
        let mut me = User::default();
        me.username = "bob".into();
        me.notify_props.insert("channel".into(), "true".into());
        for attachment in [
            json!({"text":"hi @bob"}),
            json!({"pretext":"hi @bob"}),
            json!({"title":"hi @bob"}),
            json!({"fields":[{"title":"hi @bob"}]}),
            json!({"fields":[{"value":"hi @bob"}]}),
        ] {
            let mut post = Post::default();
            post.props.insert("attachments".into(), json!([attachment]));
            assert!(desktop_mentions(
                &post,
                ChannelType::Group,
                "mention",
                true,
                &me,
                Some("on")
            ));
        }
        let mut post = Post::default();
        post.props.insert(
            "attachments".into(),
            json!([{"text":"@channel @all @here"}]),
        );
        assert!(!desktop_mentions(
            &post,
            ChannelType::Group,
            "mention",
            true,
            &me,
            Some("on")
        ));
        assert!(desktop_mentions(
            &post,
            ChannelType::Group,
            "mention",
            true,
            &me,
            Some("default")
        ));
        me.notify_props.insert("channel".into(), "false".into());
        assert!(!desktop_mentions(
            &post,
            ChannelType::Group,
            "mention",
            true,
            &me,
            Some("default")
        ));
        assert!(desktop_mentions(
            &post,
            ChannelType::Group,
            "mention",
            true,
            &me,
            Some("off")
        ));
        post.message = "@bob".into();
        assert!(desktop_mentions(
            &post,
            ChannelType::Group,
            "mention",
            true,
            &me,
            Some("on")
        ));
    }

    #[test]
    fn mute_visibility_and_dnd_override_every_desktop_preference() {
        for desktop in ["all", "mention", "none", "default"] {
            for (muted, visible, status) in [
                (true, false, Status::Online),
                (false, true, Status::Online),
                (false, false, Status::DoNotDisturb),
            ] {
                assert!(!should_notify(
                    desktop, "all", true, true, muted, visible, status
                ));
            }
        }
    }
}
