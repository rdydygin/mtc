//! Settings: appearance, zoom, and updates, with the version of this build.
//!
//! Each setting reads and writes the same preference as its menu command, so
//! the menu bar, ⌘K, and this pane always agree. The pane is a view of its
//! own so it follows the updater while a check runs.

use std::time::{SystemTime, UNIX_EPOCH};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, AxisExt as _, Disableable as _, Sizable as _, StyledExt as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    group_box::GroupBoxVariant,
    h_flex,
    setting::{
        NumberFieldOptions, RenderOptions, SettingField, SettingGroup, SettingItem, SettingPage,
        Settings,
    },
    spinner::Spinner,
    v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render,
    SharedString, Styled as _, Subscription, Window, div, prelude::FluentBuilder as _,
};

use crate::{
    actions::{ZOOM_IN_KEY, ZOOM_OUT_KEY, shortcut_label},
    appearance,
    persist::Appearance,
    time,
    update::{self, UpdateStatus, Updater},
};

/// Open the settings in `window`, in place of any other dialog.
pub fn open(window: &mut Window, cx: &mut App) {
    window.close_all_dialogs(cx);
    let view = cx.new(SettingsView::new);
    window.open_dialog(cx, move |dialog, window, _| {
        let rem = window.rem_size();
        // Dialog accepts pixels and clamps these rem-derived sizes to the viewport.
        dialog
            .title("Settings")
            // Wide enough that each setting sits beside its control.
            .w(rem * 54.)
            .child(div().h(rem * 24.).child(view.clone()))
    });
}

pub struct SettingsView {
    updater: Entity<Updater>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    fn new(cx: &mut Context<Self>) -> Self {
        let updater = update::updater(cx);
        let subscriptions = vec![cx.observe(&updater, |_, _, cx| cx.notify())];
        Self {
            updater,
            _subscriptions: subscriptions,
        }
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        Settings::new("settings")
            .sidebar_width(rem * 11.)
            .sidebar_size_range(rem * 9.0..rem * 16.)
            .with_group_variant(GroupBoxVariant::Outline)
            .page(general_page())
            .page(updates_page(self.updater.clone(), cx))
    }
}

fn general_page() -> SettingPage {
    let appearances = vec![
        ("system".into(), "Match system".into()),
        ("light".into(), "Light".into()),
        ("dark".into(), "Dark".into()),
    ];
    let appearance = SettingItem::new(
        "Appearance",
        SettingField::dropdown(
            appearances,
            |_| appearance_key(appearance::current()).into(),
            |key: SharedString, cx| appearance::set(appearance_from_key(&key), cx),
        )
        .default_value(appearance_key(Appearance::System)),
    );
    let text_size = SettingItem::new(
        "Text size",
        SettingField::number_input(
            NumberFieldOptions {
                min: appearance::MIN_FONT_SIZE.into(),
                max: appearance::MAX_FONT_SIZE.into(),
                step: 1.,
            },
            |cx| appearance::font_size(cx).into(),
            |size, cx| appearance::set_font_size(size as f32, cx),
        )
        .default_value(f64::from(appearance::DEFAULT_FONT_SIZE)),
    )
    .description(format!(
        "In pixels. {} and {} also zoom in and out.",
        shortcut_label(ZOOM_IN_KEY),
        shortcut_label(ZOOM_OUT_KEY)
    ))
    .keywords(["zoom", "font"]);

    SettingPage::new("General")
        .icon(IconName::Settings)
        .default_open(true)
        .group(SettingGroup::new().item(appearance).item(text_size))
}

fn updates_page(updater: Entity<Updater>, cx: &App) -> SettingPage {
    let version = SettingItem::render(move |options, _, cx| version_row(&updater, options, cx))
        .keywords(["version", "about", "check for updates"]);
    let automatic = SettingItem::new(
        "Update automatically",
        SettingField::switch(|_| update::is_automatic(), update::set_automatic).default_value(true),
    )
    .description(if update::can_install(cx) {
        "Check for new versions every six hours and install them in the background."
    } else {
        "Check for new versions every six hours."
    });

    SettingPage::new("Updates")
        .icon(IconName::CircleArrowUp)
        .group(SettingGroup::new().item(version).item(automatic))
}

/// The version of this build, what the last check found, and what to do
/// about it.
fn version_row(updater: &Entity<Updater>, options: &RenderOptions, cx: &mut App) -> AnyElement {
    let theme = cx.theme();
    let updater = updater.read(cx);
    let status = updater.status().clone();
    let busy = matches!(
        status,
        UpdateStatus::Checking | UpdateStatus::Downloading { .. }
    );
    let (message, is_error): (Option<SharedString>, bool) = match &status {
        UpdateStatus::Checking => (Some("Checking for updates…".into()), false),
        UpdateStatus::Downloading { version } => {
            (Some(format!("Downloading mtc {version}…").into()), false)
        }
        UpdateStatus::Ready { version } => (
            Some(
                format!("mtc {version} is installed. It starts the next time you open mtc.").into(),
            ),
            false,
        ),
        UpdateStatus::Available { version, .. } => {
            (Some(format!("mtc {version} is available.").into()), false)
        }
        UpdateStatus::Idle => match (updater.error(), updater.checked_at()) {
            (Some(error), _) => (
                Some(format!("Couldn’t check for updates. {error}").into()),
                true,
            ),
            (None, Some(at)) => (Some(up_to_date_message(at).into()), false),
            (None, None) => (None, false),
        },
    };

    let action = match status {
        UpdateStatus::Ready { .. } => Button::new("settings-restart-to-update")
            .primary()
            .label("Restart")
            .on_click(|_, _, cx| update::restart(cx)),
        UpdateStatus::Available { page, .. } => Button::new("settings-download-update")
            .outline()
            .label("Download")
            .on_click(move |_, _, cx| cx.open_url(&page)),
        _ => Button::new("settings-check-for-updates")
            .outline()
            .label("Check for updates")
            .loading(busy)
            .disabled(busy || options.is_disabled())
            // The result shows here, so no notification repeats it.
            .on_click(|_, _, cx| {
                update::updater(cx).update(cx, |updater, cx| updater.check(false, cx))
            }),
    }
    .with_size(options.size());

    let text = v_flex()
        .min_w_0()
        .child(div().text_sm().child(format!("mtc {}", update::version())))
        .when_some(message, |this, message| {
            this.child(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .text_color(if is_error {
                        theme.danger
                    } else {
                        theme.muted_foreground
                    })
                    .when(busy, |this| this.child(Spinner::new().xsmall()))
                    .child(div().min_w_0().child(message)),
            )
        });

    div()
        .w_full()
        .gap_3()
        .map(|this| {
            if options.layout().is_horizontal() {
                this.h_flex()
                    .justify_between()
                    .items_center()
                    .child(text.flex_1())
                    .child(action)
            } else {
                this.v_flex().child(text).child(action)
            }
        })
        .into_any_element()
}

fn up_to_date_message(at: SystemTime) -> String {
    let millis = at
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64);
    if time::day(millis) == time::day(now) {
        format!("Up to date. Last checked at {}.", time::clock(millis))
    } else {
        format!("Up to date. Last checked {}.", time::full(millis))
    }
}

fn appearance_key(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::System => "system",
        Appearance::Light => "light",
        Appearance::Dark => "dark",
    }
}

fn appearance_from_key(key: &str) -> Appearance {
    match key {
        "light" => Appearance::Light,
        "dark" => Appearance::Dark,
        _ => Appearance::System,
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::{TestEventExt as _, TestQueryExt as _, TestWindowExt as _};
    use gpui_kit::{
        AnyWindowHandle, Bounds, MouseButton, TestAppContext, WindowBounds, WindowOptions, px, size,
    };
    use semver::Version;

    use super::*;

    struct Host;

    impl Render for Host {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full()
        }
    }

    fn open_settings(cx: &mut TestAppContext) -> AnyWindowHandle {
        cx.update(|cx| {
            gpui_kit::init(cx);
            update::init(cx);
            crate::actions::init(cx);
        });
        let (handle, _) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1000.), px(700.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |_, cx| cx.new(|_| Host),
            )
            .expect("open test window")
        });
        cx.update_window(handle, |_, window, cx| {
            open(window, cx);
            window.render_frame(cx);
        })
        .unwrap();
        handle
    }

    fn click_label(window: &mut Window, label: &str, cx: &mut App) {
        let center = window.find_by_label(label).bounds().center();
        window.pointer_down(center, MouseButton::Left, cx);
        window.pointer_up(center, MouseButton::Left, cx);
    }

    fn show_updates(window: &mut Window, cx: &mut App) {
        // The sidebar's second page.
        window.within("settings-sidebar").click("0-1", cx);
    }

    #[gpui_kit::test]
    fn general_settings_show_the_current_preferences(cx: &mut TestAppContext) {
        let handle = open_settings(cx);
        cx.update_window(handle, |_, window, _| {
            assert!(
                window
                    .try_find_by_label("Appearance: Match system")
                    .is_some()
            );
            assert!(window.try_find_by_label("Text size").is_some());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn the_updates_page_turns_automatic_updates_off(cx: &mut TestAppContext) {
        let handle = open_settings(cx);
        assert!(update::is_automatic());
        cx.update_window(handle, |_, window, cx| {
            show_updates(window, cx);
            assert!(window.try_find("settings-check-for-updates").is_some());
            click_label(window, "Update automatically", cx);
        })
        .unwrap();
        assert!(!update::is_automatic());
        cx.update(|cx| update::set_automatic(true, cx));
    }

    #[gpui_kit::test]
    fn an_installed_update_offers_a_restart_in_place_of_a_check(cx: &mut TestAppContext) {
        let handle = open_settings(cx);
        cx.update_window(handle, |_, window, cx| show_updates(window, cx))
            .unwrap();
        let updater = cx.update(|cx| update::updater(cx));
        updater.update(cx, |updater, cx| {
            updater.installed(Version::new(9, 0, 0), cx)
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("settings-restart-to-update").is_some());
            assert!(window.try_find("settings-check-for-updates").is_none());
        })
        .unwrap();
    }
}
