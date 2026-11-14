//! The scrolling list of a conversation's posts.
//!
//! This is GPUI Kit's `MessageScroller` with its list aligned to the bottom:
//! a conversation shorter than its viewport rests on the composer, where the
//! next message will land, instead of hanging from the header. The kit's
//! scroller lays out from the top, as an assistant's transcript reads, and
//! has no setting for it.

use std::{ops::Range, time::Duration};

use gpui_kit::assets::IconName;
use gpui_kit::base::{
    ElementExt as _, ScrollBounce,
    motion::{Transition, transition},
};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _,
    button::{Button, ButtonVariants as _},
    scroll::ScrollableElement as _,
};
use gpui_kit::{
    AnyElement, App, Context, ElementId, Entity, FollowMode, InteractiveElement as _, IntoElement,
    ListAlignment, ListOffset, ListState, ParentElement as _, Pixels, RenderOnce, Role,
    StatefulInteractiveElement as _, Styled as _, Window, div, linear_color_stop, linear_gradient,
    list, prelude::FluentBuilder as _, px, rems,
};

/// How far past the viewport's edges rows are laid out ahead of a scroll.
const OVERDRAW: Pixels = px(400.);
/// How long the jump button and the bottom fade take to come and go.
const FADE: Duration = Duration::from_millis(200);

/// Where a [`PostScroller`] is scrolled to. The posts stay with the
/// timeline; the row renderer reads them.
pub struct PostScrollerState {
    list: ListState,
    /// Whether the last layout had every row on screen. Not known, and so
    /// `false`, from a change to the rows until they are next laid out.
    fits: bool,
}

impl PostScrollerState {
    /// A state for `count` rows that follows the newest.
    pub fn new(count: usize, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(count, ListAlignment::Bottom, OVERDRAW);
        list.set_follow_mode(FollowMode::Tail);

        let this = cx.weak_entity();
        list.set_scroll_handler(move |_, _, cx| {
            // The list is borrowed while it reports a scroll.
            let this = this.clone();
            cx.defer(move |cx| {
                let _ = this.update(cx, |_, cx| cx.notify());
            });
        });

        Self { list, fits: false }
    }

    /// The row at the top of the viewport and how far into it the list is
    /// scrolled. A list resting at its end reports the row count instead,
    /// whichever row is at the top.
    pub fn logical_scroll_top(&self) -> ListOffset {
        self.list.logical_scroll_top()
    }

    /// Whether every row is on screen: the conversation is no taller than
    /// its viewport. Observers are notified when it comes to fit.
    pub fn fits(&self) -> bool {
        self.fits
    }

    /// Whether the reader has scrolled away from the newest post.
    fn is_scrolled_up(&self) -> bool {
        self.list.max_offset_for_scrollbar().y > px(0.)
            && !self.list.is_following_tail()
            && !self.list.is_scrolled_to_end().unwrap_or(false)
    }

    /// Rebuild rows, retaining the reader's post and offset when it survives.
    /// Without an anchor, follow the newest (including the initial load).
    pub fn reset_with_anchor(
        &mut self,
        count: usize,
        anchor: Option<ListOffset>,
        cx: &mut Context<Self>,
    ) {
        self.list.reset(count);
        self.list.set_follow_mode(FollowMode::Tail);
        if let Some(anchor) = anchor {
            self.list.scroll_to(anchor);
        }
        self.fits = false;
        cx.notify();
    }

    /// Replace the rows in `old` with `count` new ones. A range outside the
    /// list is ignored.
    pub fn splice(&mut self, old: Range<usize>, count: usize, cx: &mut Context<Self>) {
        if !self.contains(&old) {
            return;
        }
        let before = old.start.checked_sub(1);
        let touches_start = old.start == 0;
        self.list.splice(old, count);

        // The list's insets ride on its first and last rows, so a row that
        // has just become or stopped being one of them has a stale height.
        if let Some(last) = self.list.item_count().checked_sub(1) {
            self.list.remeasure_items(last..last + 1);
            if let Some(before) = before.filter(|before| *before != last) {
                self.list.remeasure_items(before..before + 1);
            }
        }
        if touches_start {
            for ix in [0, count] {
                if ix < self.list.item_count() {
                    self.list.remeasure_items(ix..ix + 1);
                }
            }
        }

        self.fits = false;
        cx.notify();
    }

    /// Measure the rows in `range` again, keeping the scroll position. A
    /// range outside the list is ignored.
    pub fn remeasure_items(&mut self, range: Range<usize>, cx: &mut Context<Self>) {
        if self.contains(&range) {
            self.list.remeasure_items(range);
            cx.notify();
        }
    }

    /// Bring row `ix` to the top of the viewport, if there is such a row.
    pub fn scroll_to_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.list.item_count() {
            self.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
    }

    /// Go to the newest row and follow it again.
    pub fn scroll_to_end(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(FollowMode::Tail);
        self.list.scroll_to_end();
        cx.notify();
    }

    pub(crate) fn is_following_tail(&self) -> bool {
        self.list.is_following_tail()
    }

    #[cfg(test)]
    pub(crate) fn item_count(&self) -> usize {
        self.list.item_count()
    }

    fn contains(&self, range: &Range<usize>) -> bool {
        range.start <= range.end && range.end <= self.list.item_count()
    }
}

/// Builds the row at an index of the list.
type RenderRow = Box<dyn FnMut(usize, &mut Window, &mut App) -> AnyElement>;

/// The posts of a conversation, from the composer up: a virtualized list
/// with a scrollbar, a fade over the rows cut off at the bottom, and a
/// button back to the newest post.
#[derive(IntoElement)]
pub struct PostScroller {
    id: ElementId,
    state: Entity<PostScrollerState>,
    render_row: RenderRow,
}

impl PostScroller {
    pub fn new(
        id: impl Into<ElementId>,
        state: Entity<PostScrollerState>,
        render_row: impl FnMut(usize, &mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        Self {
            id: id.into(),
            state,
            render_row: Box::new(render_row),
        }
    }
}

impl RenderOnce for PostScroller {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let (list_state, scrolled_up) = {
            let state = self.state.read(cx);
            (state.list.clone(), state.is_scrolled_up())
        };
        // The fade and the jump button show only away from the newest post:
        // at the end nothing is cut off, and there is nowhere to jump to.
        let away = transition(
            (self.id.clone(), "scrolled-up"),
            if scrolled_up { 1. } else { 0. },
            Transition::new(FADE),
            window,
            cx,
        );
        let theme = cx.theme();

        // Read outside the row closure: the list is borrowed while it
        // renders its rows.
        let count = list_state.item_count();
        let mut render_row = self.render_row;
        let rows = list(list_state.clone(), move |ix, window, cx| {
            // The insets are on the rows, not the list: a list measures its
            // scroll range from its rows alone, so padding of its own would
            // leave the end of the range out of reach.
            div()
                .w_full()
                .when(ix == 0, |this| this.pt_2())
                .when(ix + 1 == count, |this| this.pb_2())
                .child(render_row(ix, window, cx))
                .into_any_element()
        })
        .size_full()
        .min_h_0();

        let viewport = div()
            .id((self.id.clone(), "viewport"))
            // New posts are announced as a log's are.
            .role(Role::Log)
            .size_full()
            .min_h_0()
            .min_w_0()
            .child(
                // The bounce scrolls the list directly, past the list's own
                // scroll handler; tell the state, as that handler would.
                ScrollBounce::new((self.id.clone(), "bounce"), &list_state, rows).on_scroll({
                    let state = self.state.downgrade();
                    move |_, cx| {
                        let _ = state.update(cx, |_, cx| cx.notify());
                    }
                }),
            )
            // After the list, which has laid its rows out by now. A viewport
            // with no height shows no rows, however few there are.
            .on_prepaint({
                let state = self.state.clone();
                let list_state = list_state.clone();
                move |bounds, _, cx| {
                    let fits = bounds.size.height > px(0.)
                        && list_state.max_offset_for_scrollbar().y <= px(0.);
                    if state.read(cx).fits != fits {
                        // Observers act on this, so tell them after the frame.
                        cx.defer(move |cx| {
                            state.update(cx, |state, cx| {
                                state.fits = fits;
                                if fits {
                                    cx.notify();
                                }
                            });
                        });
                    }
                }
            })
            // Above the rows, below the scrollbar and the jump button.
            .when(away > 0., |this| {
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom_0()
                        .h(rems(3.))
                        .opacity(away)
                        .bg(linear_gradient(
                            180.,
                            linear_color_stop(theme.background.opacity(0.), 0.),
                            linear_color_stop(theme.background, 1.),
                        )),
                )
            })
            .vertical_scrollbar(&list_state);

        div()
            .id(self.id.clone())
            .relative()
            .size_full()
            .min_h_0()
            .overflow_hidden()
            .child(viewport)
            .when(away > 0., |this| {
                let state = self.state.clone();
                this.child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .bottom(rems(0.5 + away * 0.5))
                        .flex()
                        .justify_center()
                        .opacity(away)
                        .child(
                            Button::new((self.id, "jump-to-latest"))
                                .secondary()
                                .icon(IconName::ArrowDown)
                                .tooltip("Jump to latest")
                                .rounded(theme.radius_full())
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.background)
                                .text_color(theme.foreground)
                                // Fading out, it is no longer there to press.
                                .disabled(!scrolled_up)
                                .on_click(move |_, _, cx| {
                                    state.update(cx, |state, cx| state.scroll_to_end(cx));
                                }),
                        ),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::base::TestSupportExt as _;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, AppContext as _, Bounds, Render, ScrollDelta, TestAppContext,
        WindowBounds, WindowOptions, point, size,
    };

    use super::*;

    const ROW: Pixels = px(40.);
    const VIEWPORT: Pixels = px(400.);

    struct Posts {
        scroller: Entity<PostScrollerState>,
    }

    impl Render for Posts {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("posts")
                .test_support()
                .h(VIEWPORT)
                .child(PostScroller::new(
                    "scroller",
                    self.scroller.clone(),
                    |ix, _, _| {
                        div()
                            .id(("row", ix))
                            .test_support()
                            .h(ROW)
                            .into_any_element()
                    },
                ))
        }
    }

    /// A window showing `count` rows, drawn once.
    fn open(count: usize, cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<PostScrollerState>) {
        cx.update(gpui_kit::init);
        let (window, view) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: point(px(0.), px(0.)),
                    size: size(px(600.), px(600.)),
                })),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |_, cx| {
                cx.new(|cx| Posts {
                    scroller: cx.new(|cx| PostScrollerState::new(count, cx)),
                })
            })
            .unwrap()
        });
        cx.run_until_parked();
        let scroller = view.read_with(cx, |view, _| view.scroller.clone());
        (window, scroller)
    }

    #[gpui_kit::test]
    fn a_short_conversation_rests_on_the_bottom_edge(cx: &mut TestAppContext) {
        let (window, _) = open(2, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let posts = window.find("posts").bounds();
            let first = window.find(("row", 0usize)).bounds();
            let last = window.find(("row", 1usize)).bounds();
            // The last row sits its inset above the bottom, the first on it.
            assert_eq!(last.bottom(), posts.bottom() - px(8.));
            assert_eq!(first.bottom(), last.top());
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn a_long_conversation_shows_its_newest_rows(cx: &mut TestAppContext) {
        let (window, scroller) = open(30, cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let posts = window.find("posts").bounds();
            let last = window.find(("row", 29usize)).bounds();
            assert_eq!(last.bottom(), posts.bottom() - px(8.));
            assert!(window.try_find(("row", 0usize)).is_none());
        })
        .unwrap();
        scroller.read_with(cx, |scroller, _| assert!(!scroller.fits()));
    }

    #[gpui_kit::test]
    fn scrolling_up_leaves_the_newest_row_until_scrolled_back(cx: &mut TestAppContext) {
        let (window, scroller) = open(30, cx);
        let wheel = |by: Pixels, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.scroll("posts", ScrollDelta::Pixels(point(px(0.), by)), cx);
                window.render_frame(cx);
            })
            .unwrap();
            cx.run_until_parked();
        };
        let newest_bottom = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| {
                let posts = window.find("posts").bounds();
                window
                    .try_find(("row", 29usize))
                    .map(|row| posts.bottom() - row.bounds().bottom())
            })
            .unwrap()
        };

        wheel(ROW * 3., cx);
        assert_eq!(newest_bottom(cx), None, "scrolled out of view");
        scroller.read_with(cx, |scroller, _| assert!(scroller.is_scrolled_up()));

        // A row arriving doesn't pull a reader who has scrolled away.
        scroller.update(cx, |scroller, cx| scroller.splice(30..30, 1, cx));
        cx.run_until_parked();
        scroller.read_with(cx, |scroller, _| assert!(scroller.is_scrolled_up()));

        wheel(-ROW * 10., cx);
        scroller.read_with(cx, |scroller, _| assert!(!scroller.is_scrolled_up()));
        cx.update_window(window, |_, window, _| {
            let posts = window.find("posts").bounds();
            let newest = window.find(("row", 30usize)).bounds();
            assert_eq!(newest.bottom(), posts.bottom() - px(8.));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn rows_that_all_fit_are_reported_once_they_are_laid_out(cx: &mut TestAppContext) {
        let (_, scroller) = open(2, cx);
        let reports = std::rc::Rc::new(std::cell::Cell::new(0));
        cx.update(|cx| {
            let reports = reports.clone();
            cx.observe(&scroller, move |scroller, cx| {
                if scroller.read(cx).fits() {
                    reports.set(reports.get() + 1);
                }
            })
            .detach();
        });
        scroller.read_with(cx, |scroller, _| assert!(scroller.fits()));

        // Older rows arrive and still fit: not known until they are laid
        // out, then reported again.
        scroller.update(cx, |scroller, cx| {
            scroller.splice(0..0, 3, cx);
            assert!(!scroller.fits());
        });
        cx.run_until_parked();
        scroller.read_with(cx, |scroller, _| assert!(scroller.fits()));
        assert_eq!(reports.get(), 1);

        // More than the viewport holds.
        scroller.update(cx, |scroller, cx| scroller.splice(0..0, 20, cx));
        cx.run_until_parked();
        scroller.read_with(cx, |scroller, _| assert!(!scroller.fits()));
        assert_eq!(reports.get(), 1);
    }
}
