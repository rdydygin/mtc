//! The "Ann is typing…" line above a conversation's composer.
//!
//! Its own view, so a typing signal notifies this line alone, and only
//! when the signal is for the conversation it shows: a signal for any
//! other conversation draws nothing at all.

use gpui_kit::component::ActiveTheme as _;
use gpui_kit::{
    Context, Entity, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, Window, div,
};
use session::{Directory, Session, TimelineKind, Typing, TypingChanged};

pub struct TypingIndicator {
    kind: TimelineKind,
    typing: Entity<Typing>,
    directory: Entity<Directory>,
    _subscriptions: [Subscription; 2],
}

impl TypingIndicator {
    pub fn new(kind: TimelineKind, session: &Entity<Session>, cx: &mut Context<Self>) -> Self {
        let session = session.read(cx);
        let typing = session.typing().clone();
        let directory = session.directory().clone();
        let _subscriptions = [
            cx.subscribe(&typing, |this, _, TypingChanged(kind), cx| {
                if kind == &this.kind {
                    cx.notify();
                }
            }),
            // A name that arrives late; nothing to redraw with no one typing.
            cx.observe(&directory, |this, _, cx| {
                if this.typing.read(cx).users(&this.kind).next().is_some() {
                    cx.notify();
                }
            }),
        ];
        Self {
            kind,
            typing,
            directory,
            _subscriptions,
        }
    }
}

impl Render for TypingIndicator {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let directory = self.directory.read(cx);
        let names: Vec<SharedString> = self
            .typing
            .read(cx)
            .users(&self.kind)
            .map(|id| directory.display_name(id))
            .collect();
        let text = match names.as_slice() {
            [] => String::new(),
            [one] => format!("{one} is typing…"),
            [a, b] => format!("{a} and {b} are typing…"),
            _ => "Several people are typing…".to_string(),
        };
        // Fixed height: the composer must not jump when someone types.
        div()
            .flex_none()
            .h_5()
            .px_5()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .truncate()
            .child(text)
    }
}
