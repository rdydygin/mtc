//! Placeholders in the shape of content that is still on its way.
//!
//! Each one reserves the same lanes as the real thing (the sidebar row's
//! leading icon and label, a post's avatar, name and lines, the header's
//! icon and title, the composer's field), so nothing moves when the content
//! arrives. They are shown only while a request is actually outstanding.

use gpui_kit::component::{ActiveTheme as _, TitleBar, h_flex, skeleton::Skeleton, v_flex};
use gpui_kit::{App, IntoElement, ParentElement as _, Styled as _, div, relative, rems};

use crate::persist;

/// Label widths for placeholder rows, as fractions of the space available.
/// Fixed, so the placeholders don't shimmer into new shapes on each frame,
/// and varied, so they read as a list of names rather than a grid.
const LABEL_WIDTHS: [f32; 9] = [0.62, 0.45, 0.78, 0.52, 0.68, 0.4, 0.72, 0.56, 0.48];

/// Placeholder posts: (name width in rems, line widths as fractions). The
/// last is the newest, nearest the composer.
const POSTS: [(f32, &[f32]); 6] = [
    (6.5, &[0.82, 0.46]),
    (8., &[0.58]),
    (5.5, &[0.9, 0.74, 0.32]),
    (7., &[0.4]),
    (9., &[0.76, 0.52]),
    (6., &[0.64]),
];

/// Sidebar conversation rows, matching `SidebarView::channel_item`.
pub fn conversation_rows(count: usize, offset: usize, cx: &App) -> impl IntoElement {
    let radius = cx.theme().radius;
    v_flex().gap_0p5().children((0..count).map(move |ix| {
        let width = LABEL_WIDTHS[(ix + offset) % LABEL_WIDTHS.len()];
        h_flex()
            .h_9()
            .px_2()
            .gap_2()
            .child(Skeleton::new().size_6().flex_none().rounded(radius))
            .child(
                div().flex_1().min_w_0().child(
                    Skeleton::new()
                        .secondary()
                        .h_3()
                        .w(relative(width))
                        .rounded(radius),
                ),
            )
    }))
}

/// One placeholder bar for the team name in the sidebar header.
pub fn team_name(cx: &App) -> impl IntoElement {
    Skeleton::new()
        .h_4()
        .w(relative(0.5))
        .rounded(cx.theme().radius)
}

/// Posts of a conversation, matching `PostRow`'s avatar and text lanes and
/// gathered at the bottom, where the newest messages are.
pub fn posts(cx: &App) -> impl IntoElement {
    let radius = cx.theme().radius;
    v_flex()
        .flex_1()
        .min_h_0()
        .overflow_hidden()
        .justify_end()
        .pb_2()
        .children(POSTS.iter().map(move |(name, lines)| {
            h_flex()
                .w_full()
                .items_start()
                .gap_3()
                .px_4()
                .pt_3()
                .pb_1()
                .child(Skeleton::new().size_9().flex_none().rounded_full())
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .gap_2()
                        .pt_0p5()
                        .child(Skeleton::new().h_3().w(rems(*name)).rounded(radius))
                        .children(lines.iter().map(move |width| {
                            Skeleton::new()
                                .secondary()
                                .h_3()
                                .w(relative(*width))
                                .rounded(radius)
                        })),
                )
        }))
}

/// A conversation's header bar, matching `TimelineView::render_header`.
fn header(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .flex_none()
        .h_16()
        .pl_4()
        .pr_4()
        .gap_3()
        .border_b_1()
        .border_color(theme.border)
        .child(Skeleton::new().size_4().flex_none().rounded(theme.radius))
        .child(
            v_flex()
                .flex_1()
                .gap_1p5()
                .child(Skeleton::new().h_4().w(rems(10.)).rounded(theme.radius))
                .child(
                    Skeleton::new()
                        .secondary()
                        .h_3()
                        .w(rems(16.))
                        .max_w_full()
                        .rounded(theme.radius),
                ),
        )
}

/// The composer's field, matching `Composer`'s inset and corner radius.
fn composer(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    div()
        .flex_none()
        // The typing line above the composer.
        .pt_5()
        .px_4()
        .pb_4()
        .child(div().h_12().rounded(theme.radius_lg).bg(theme.secondary))
}

/// A whole conversation: header, posts, composer.
pub fn conversation(cx: &App) -> impl IntoElement {
    v_flex()
        .size_full()
        .child(header(cx))
        .child(posts(cx))
        .child(composer(cx))
}

/// The signed-in window before the workspace exists, in the workspace's
/// layout: the team rail, the sidebar, and a conversation, on the same
/// backdrop and with the same gutters.
pub fn workspace(cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let sidebar_width = persist::read_with(|state| state.sidebar_width_rem).unwrap_or(21.);
    let has_conversation = persist::read_with(|state| state.last_channel.is_some());
    let inset = |child: gpui_kit::AnyElement| {
        div()
            .size_full()
            .min_w_0()
            .rounded(theme.radius_lg)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .overflow_hidden()
            .child(child)
    };
    v_flex()
        .size_full()
        .bg(theme.title_bar)
        .child(TitleBar::new())
        .child(
            h_flex()
                .flex_1()
                .min_h_0()
                .items_stretch()
                .child(
                    v_flex()
                        .w(rems(3.5))
                        .flex_none()
                        .items_center()
                        .py_3()
                        .gap_3()
                        .child(Skeleton::new().size_10().rounded(theme.radius)),
                )
                .child(
                    div()
                        .w(rems(sidebar_width - 3.5))
                        .flex_none()
                        .py_2()
                        .pr_2()
                        .child(inset(
                            v_flex()
                                .size_full()
                                .px_2()
                                .gap_4()
                                .child(div().px_2().pt_4().pb_10().child(team_name(cx)))
                                .child(conversation_rows(6, 0, cx))
                                .child(conversation_rows(4, 6, cx))
                                .into_any_element(),
                        )),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .py_2()
                        .pr_2()
                        .child(inset(if has_conversation {
                            conversation(cx).into_any_element()
                        } else {
                            div().into_any_element()
                        })),
                ),
        )
}
