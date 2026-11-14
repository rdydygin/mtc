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

/// Add an extension. Later registrations take precedence for slash commands
/// with the same name.
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
                let is_visible =
                    window.is_window_active() && session.active_channel() == Some(&post.channel_id);
                let status = session.directory().read(cx).status(&session.me().id);
                if status == mattermost::Status::DoNotDisturb
                    || (!*mentions_me && !is_direct)
                    || is_visible
                    || channel.is_muted()
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
