//! The "Ann is typing…" line above a conversation's composer.
//!
//! Its own view, so a typing signal notifies this line alone, and only
//! when the signal is for the conversation it shows: a signal for any
//! other conversation draws nothing at all.
//!
//! The line looks up the names it shows, and only while it is drawn. The
//! directory redraws everything that shows users when a lookup answers, so
//! asking for every typer in every conversation would undo the point.

use std::collections::HashSet;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _, Render, Role,
    StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};
use mattermost::UserId;
use session::{Directory, Session, TimelineKind, Typing, TypingChanged};

/// More typers than this read "Several people are typing…", without names.
const NAMED: usize = 2;

pub struct TypingIndicator {
    kind: TimelineKind,
    typing: Entity<Typing>,
    directory: Entity<Directory>,
    /// Typers already looked up, so a name the server can't give is asked
    /// for once, not again with every signal.
    asked: HashSet<UserId>,
    /// The directory's names when this line last looked, so presence and
    /// avatar changes don't redraw it.
    names_revision: u64,
    _subscriptions: [Subscription; 2],
}

impl TypingIndicator {
    pub fn new(kind: TimelineKind, session: &Entity<Session>, cx: &mut Context<Self>) -> Self {
        let session = session.read(cx);
        let typing = session.typing().clone();
        let directory = session.directory().clone();
        let names_revision = directory.read(cx).names_revision();
        let _subscriptions = [
            cx.subscribe(&typing, |this, _, TypingChanged(kind), cx| {
                if kind == &this.kind {
                    cx.notify();
                }
            }),
            // A name that arrives late; nothing to redraw with no one typing.
            cx.observe(&directory, |this, directory, cx| {
                let revision = directory.read(cx).names_revision();
                if revision != this.names_revision {
                    this.names_revision = revision;
                    if this.typing.read(cx).users(&this.kind).next().is_some() {
                        cx.notify();
                    }
                }
            }),
        ];
        Self {
            kind,
            typing,
            directory,
            asked: HashSet::new(),
            names_revision,
            _subscriptions,
        }
    }
}

impl Render for TypingIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let typers: Vec<UserId> = self.typing.read(cx).users(&self.kind).cloned().collect();
        let directory = self.directory.read(cx);
        let text = match typers.as_slice() {
            [] => String::new(),
            [one] => format!("{} is typing…", directory.display_name(one)),
            [a, b] => format!(
                "{} and {} are typing…",
                directory.display_name(a),
                directory.display_name(b)
            ),
            _ => "Several people are typing…".to_string(),
        };
        // Look up the names shown here, once each, after this frame.
        if typers.len() <= NAMED {
            let unknown: Vec<UserId> = typers
                .into_iter()
                .filter(|id| directory.user(id).is_none() && !self.asked.contains(id))
                .collect();
            if !unknown.is_empty() {
                self.asked.extend(unknown.iter().cloned());
                let directory = self.directory.clone();
                cx.defer(move |cx| directory.update(cx, |d, cx| d.ensure(unknown, cx)));
            }
        }
        // Fixed height: the composer must not jump when someone types.
        div()
            .id("typing")
            .test_support()
            .role(Role::Status)
            .aria_label(text.clone())
            .flex_none()
            .h_5()
            .px_5()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .truncate()
            .child(text)
    }
}
