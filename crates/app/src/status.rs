//! Setting the user's custom status ("🗓 In a meeting · clears in 1 hour").

use gpui_kit::component::{
    ActiveTheme as _, IndexPath, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    select::{Select, SelectEvent, SelectState},
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, Focusable as _, Hsla, IntoElement,
    ParentElement as _, Render, SharedString, Styled as _, Subscription, Window, div, img,
    prelude::FluentBuilder as _,
};
use mattermost::{CustomStatus, Status};
use session::{CustomEmojis, Session};

use crate::time;

/// Emoji offered for a status, by Mattermost name.
const EMOJI: [&str; 12] = [
    "speech_balloon",
    "calendar",
    "bus",
    "house",
    "palm_tree",
    "face_with_thermometer",
    "coffee",
    "headphones",
    "rocket",
    "eyes",
    "brain",
    "zzz",
];

/// Common statuses, each with a sensible expiry.
const PRESETS: [(&str, &str, &str); 5] = [
    ("calendar", "In a meeting", "one_hour"),
    ("bus", "Commuting", "thirty_minutes"),
    ("face_with_thermometer", "Out sick", "today"),
    ("palm_tree", "On vacation", "this_week"),
    ("house", "Working remotely", "today"),
];

const CLEAR_AFTER: [(&str, &str); 6] = [
    ("Don’t clear", ""),
    ("30 minutes", "thirty_minutes"),
    ("1 hour", "one_hour"),
    ("4 hours", "four_hours"),
    ("Today", "today"),
    ("This week", "this_week"),
];

/// Presence as people read it.
pub fn presence_label(status: Status) -> &'static str {
    match status {
        Status::Online => "Online",
        Status::Away => "Away",
        Status::DoNotDisturb => "Do not disturb",
        Status::Offline => "Offline",
    }
}

/// The color of a presence dot.
pub fn presence_color(status: Status, cx: &App) -> Hsla {
    let theme = cx.theme();
    match status {
        Status::Online => theme.success,
        Status::Away => theme.warning,
        Status::DoNotDisturb => theme.danger,
        Status::Offline => theme.muted_foreground,
    }
}

/// An emoji as text: the character, or `:name:` when there is none.
pub fn emoji_char(name: &str) -> SharedString {
    let name = session::emoji_name(name);
    session::emoji_for(name)
        .map(SharedString::from)
        .unwrap_or_else(|| SharedString::from(format!(":{name}:")))
}

/// An emoji as drawn beside text: the character, or the server's image for
/// a custom emoji, sized to the surrounding text.
pub fn emoji_element(name: &str, custom: &CustomEmojis) -> AnyElement {
    let name = session::emoji_name(name);
    match custom.image_url(name) {
        Some(url) if session::emoji_for(name).is_none() => img(SharedString::from(url.clone()))
            .flex_none()
            .size_4()
            .into_any_element(),
        _ => emoji_char(name).into_any_element(),
    }
}

fn status_for_save(
    current: Option<&CustomStatus>,
    emoji: &str,
    text: String,
    expiry: Option<(&str, String)>,
) -> Option<CustomStatus> {
    if text.is_empty() {
        return None;
    }
    let mut status = current.cloned().unwrap_or_default();
    status.emoji = emoji.to_owned();
    status.text = text;
    // Only an explicit expiry choice should replace the server's deadline.
    if let Some((duration, expires_at)) = expiry {
        status.duration = duration.to_owned();
        status.expires_at = expires_at;
    }
    Some(status)
}

pub struct StatusDialog {
    session: Entity<Session>,
    emoji: String,
    current: Option<CustomStatus>,
    expiry_changed: bool,
    text: Entity<InputState>,
    clear_after: Entity<SelectState<Vec<&'static str>>>,
    _subscriptions: Vec<Subscription>,
}

impl StatusDialog {
    fn new(session: Entity<Session>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let current = {
            let session = session.read(cx);
            let me = session.me().id.clone();
            session
                .directory()
                .read(cx)
                .user(&me)
                .and_then(|u| u.custom_status(&time::now_rfc3339()))
        };
        let emoji = current
            .as_ref()
            .map(|s| s.emoji.clone())
            .unwrap_or_else(|| "speech_balloon".into());
        let text = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("What’s your status?")
                .default_value(current.as_ref().map(|s| s.text.clone()).unwrap_or_default())
        });
        let duration_ix = current.as_ref().map_or(Some(4), |s| {
            CLEAR_AFTER.iter().position(|(_, d)| *d == s.duration)
        });
        let clear_after = cx.new(|cx| {
            SelectState::new(
                CLEAR_AFTER
                    .iter()
                    .map(|(label, _)| *label)
                    .collect::<Vec<_>>(),
                duration_ix.map(IndexPath::new),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe_in(&text, window, |this, _, event, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.save(window, cx);
            }
        });
        let expiry_subscription = cx.subscribe(&clear_after, |this, _, event, _| {
            let SelectEvent::Confirm(_) = event;
            this.expiry_changed = true;
        });
        Self {
            session,
            emoji,
            expiry_changed: current.is_none(),
            current,
            text,
            clear_after,
            _subscriptions: vec![subscription, expiry_subscription],
        }
    }

    fn apply_preset(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (emoji, text, duration) = PRESETS[ix];
        self.emoji = emoji.into();
        self.expiry_changed = true;
        self.text.update(cx, |t, cx| t.set_value(text, window, cx));
        let duration_ix = CLEAR_AFTER
            .iter()
            .position(|(_, d)| *d == duration)
            .unwrap_or(0);
        self.clear_after.update(cx, |s, cx| {
            s.set_selected_index(Some(IndexPath::new(duration_ix)), window, cx)
        });
        cx.notify();
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text.read(cx).value().trim().to_string();
        let duration_ix = self
            .clear_after
            .read(cx)
            .selected_index(cx)
            .map_or(0, |ix| ix.row);
        let duration = CLEAR_AFTER.get(duration_ix).map_or("", |(_, d)| *d);
        let status = status_for_save(
            self.current.as_ref(),
            &self.emoji,
            text,
            self.expiry_changed
                .then(|| (duration, time::status_expiry(duration).unwrap_or_default())),
        );
        self.session
            .update(cx, |s, cx| s.set_custom_status(status, cx));
        window.close_dialog(cx);
    }
}

impl Render for StatusDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            .gap_4()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .size_9()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded(theme.radius)
                            .bg(theme.secondary)
                            .text_lg()
                            .child(emoji_char(&self.emoji)),
                    )
                    .child(div().flex_1().child(Input::new(&self.text))),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(EMOJI.iter().map(|name| {
                        let name = *name;
                        Button::new(SharedString::from(format!("emoji-{name}")))
                            .ghost()
                            .small()
                            .selected(self.emoji == name)
                            .toggled(self.emoji == name)
                            .label(emoji_char(name))
                            .tooltip(format!(":{name}:"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.emoji = name.into();
                                cx.notify();
                            }))
                    })),
            )
            .child(
                v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("Suggestions"),
                    )
                    .children(PRESETS.iter().enumerate().map(|(ix, (emoji, text, _))| {
                        Button::new(("status-preset", ix))
                            .ghost()
                            .accessibility_label(*text)
                            .small()
                            .w_full()
                            .justify_start()
                            .child(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .child(emoji_char(emoji))
                                    .child(*text),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.apply_preset(ix, window, cx)
                            }))
                    })),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("Clear after"),
                    )
                    .child(div().flex_1().child(Select::new(&self.clear_after).small())),
            )
    }
}

/// Open the status dialog over `window`.
pub fn open(session: Entity<Session>, window: &mut Window, cx: &mut App) {
    let has_status = {
        let s = session.read(cx);
        let me = s.me().id.clone();
        s.directory()
            .read(cx)
            .user(&me)
            .and_then(|u| u.custom_status(&time::now_rfc3339()))
            .is_some()
    };
    let dialog = cx.new(|cx| StatusDialog::new(session.clone(), window, cx));
    let focus = dialog.read(cx).text.focus_handle(cx);
    let focused = std::cell::Cell::new(false);
    window.open_dialog(cx, move |d, window, cx| {
        if !focused.replace(true) {
            focus.focus(window, cx);
        }
        let save = dialog.clone();
        let clear = session.clone();
        d.title("Set a status")
            .w(window.rem_size() * 30.)
            .child(dialog.clone())
            .footer(
                h_flex()
                    .w_full()
                    .gap_2()
                    .when(has_status, |this| {
                        this.child(
                            Button::new("clear-status")
                                .ghost()
                                .label("Clear status")
                                .on_click(move |_, window, cx| {
                                    clear.update(cx, |s, cx| s.set_custom_status(None, cx));
                                    window.close_dialog(cx);
                                }),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("cancel-status")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(Button::new("save-status").primary().label("Save").on_click(
                        move |_, window, cx| save.update(cx, |this, cx| this.save(window, cx)),
                    )),
            )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_edit_preserves_listed_and_arbitrary_emoji_and_expiry() {
        for emoji in ["calendar", "tada", "custom_server_emoji"] {
            for duration in ["one_hour", "date_and_time", "", "future_duration"] {
                let current =
                    CustomStatus::new(emoji, "Old text").expiring(duration, "2030-11-07T12:34:56Z");
                let saved =
                    status_for_save(Some(&current), &current.emoji, "New text".into(), None)
                        .expect("nonempty status");
                assert_eq!(saved.emoji, emoji);
                assert_eq!(saved.text, "New text");
                assert_eq!(saved.duration, current.duration);
                assert_eq!(saved.expires_at, current.expires_at);
            }
        }
    }

    #[test]
    fn choosing_another_emoji_preserves_expiry() {
        let current = CustomStatus::new("custom_server_emoji", "Text")
            .expiring("date_and_time", "2030-11-07T12:34:56Z");
        let saved = status_for_save(Some(&current), "rocket", current.text.clone(), None).unwrap();
        assert_eq!(saved.emoji, "rocket");
        assert_eq!(saved.duration, current.duration);
        assert_eq!(saved.expires_at, current.expires_at);
    }

    #[test]
    fn explicit_expiry_choice_replaces_or_clears_deadline() {
        let current =
            CustomStatus::new("calendar", "Text").expiring("date_and_time", "2030-11-07T12:34:56Z");
        let saved = status_for_save(
            Some(&current),
            "calendar",
            "Text".into(),
            Some(("one_hour", "2030-11-07T13:00:00Z".into())),
        )
        .unwrap();
        assert_eq!(saved.duration, "one_hour");
        assert_eq!(saved.expires_at, "2030-11-07T13:00:00Z");

        let saved = status_for_save(
            Some(&current),
            "calendar",
            "Text".into(),
            Some(("", String::new())),
        )
        .unwrap();
        assert!(saved.duration.is_empty());
        assert!(saved.expires_at.is_empty());
    }

    #[test]
    fn empty_text_clears_status() {
        assert!(status_for_save(None, "calendar", String::new(), None).is_none());
    }

    #[test]
    fn arbitrary_emoji_preview_uses_shortcode_fallback() {
        assert_eq!(emoji_char("custom_server_emoji"), ":custom_server_emoji:");
        assert_eq!(emoji_char("tada"), "🎉");
    }
}
