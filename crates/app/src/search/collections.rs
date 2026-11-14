//! Saved and channel-pinned collections share Search's result rows and open events.
use super::*;
use session::SessionEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Collection {
    Saved,
    Pinned(ChannelId),
}

impl Collection {
    fn title(&self) -> &'static str {
        match self {
            Self::Saved => "Saved",
            Self::Pinned(_) => "Pinned messages",
        }
    }
    fn empty(&self) -> &'static str {
        match self {
            Self::Saved => {
                "No saved messages. Choose Save message in a message’s menu to keep it here."
            }
            Self::Pinned(_) => {
                "No pinned messages in this channel. Choose Pin to channel in a message’s menu."
            }
        }
    }
}

impl SearchView {
    pub fn is_collection(&self) -> bool {
        self.collection.is_some()
    }

    pub fn new_collection(
        session: Entity<Session>,
        collection: Collection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let scope = match &collection {
            Collection::Saved => SearchScope::Team,
            Collection::Pinned(channel) => SearchScope::Conversation(channel.clone()),
        };
        let mut view = Self::new(session.clone(), scope, window, cx);
        view.collection = Some(collection);
        view._subscriptions
            .push(cx.subscribe(&session, |this, _, event, cx| match event {
                SessionEvent::Reconnected => this.load_collection(0, cx),
                SessionEvent::SavedChanged if this.collection == Some(Collection::Saved) => {
                    let rows = this
                        .results
                        .iter()
                        .filter(|post| this.session.read(cx).is_saved(&post.id))
                        .cloned()
                        .collect();
                    this.replace_collection_rows(rows);
                    this.load_collection(0, cx);
                }
                SessionEvent::PostChanged(post) => {
                    let relevant = match &this.collection {
                        Some(Collection::Pinned(channel)) => &post.channel_id == channel,
                        Some(Collection::Saved) => this.results.iter().any(|p| p.id == post.id),
                        None => false,
                    };
                    if relevant {
                        this.load_collection(0, cx);
                    }
                }
                _ => {}
            }));
        view._subscriptions
            .push(cx.observe(&session, |this, _, cx| {
                this.prune_collection_channels(cx);
            }));
        view.load_collection(0, cx);
        view
    }

    /// Preserve selection and the visible post (including its fractional offset),
    /// rather than scrolling to the selection when rows arrive or disappear.
    fn replace_collection_rows(&mut self, rows: Vec<Arc<Post>>) {
        let selected = self
            .selected
            .and_then(|ix| self.results.get(ix))
            .map(|p| p.id.clone());
        let mut offset = self.collection_scroll.logical_scroll_top();
        let anchor = self.results.get(offset.item_ix).map(|p| p.id.clone());
        self.result_text = rows
            .iter()
            .map(|post| {
                (
                    post.id.clone(),
                    session::emojify(&session::post_text(post), &CustomEmojis::default()),
                )
            })
            .collect();
        self.results = rows;
        self.selected = selected
            .and_then(|id| self.results.iter().position(|p| p.id == id))
            .or_else(|| {
                (!self.results.is_empty())
                    .then(|| self.selected.unwrap_or(0).min(self.results.len() - 1))
            });
        offset.item_ix = anchor
            .and_then(|id| self.results.iter().position(|p| p.id == id))
            .unwrap_or_else(|| offset.item_ix.min(self.results.len().saturating_sub(1)));
        self.collection_scroll.reset(self.results.len());
        self.collection_scroll.scroll_to(offset);
    }

    fn prune_collection_channels(&mut self, cx: &mut Context<Self>) {
        if self.collection_unavailable
            || self.session.read(cx).channels_state() != &session::LoadState::Loaded
        {
            return;
        }
        if let Some(Collection::Pinned(channel)) = &self.collection
            && self.session.read(cx).channel(channel).is_none()
        {
            self.collection_unavailable = true;
            self.revision += 1;
            self._search = None;
            self.loading_more = false;
            self.next_page = None;
            self.replace_collection_rows(Vec::new());
            cx.emit(SearchEvent::Close);
            cx.notify();
            return;
        }
        let rows: Vec<_> = self
            .results
            .iter()
            .filter(|post| self.session.read(cx).channel(&post.channel_id).is_some())
            .cloned()
            .collect();
        if rows.len() != self.results.len() {
            self.replace_collection_rows(rows);
            cx.notify();
        }
    }

    pub(super) fn load_collection(&mut self, page: usize, cx: &mut Context<Self>) {
        let Some(collection) = self.collection.clone() else {
            return;
        };
        if self.collection_unavailable {
            return;
        }
        self.revision += 1;
        let revision = self.revision;
        self.loading_more = page > 0;
        self.more_failed = false;
        if page == 0 {
            self.status = Status::Searching;
        }
        // Refresh every loaded page before publishing a replacement. A later
        // page failure leaves the previous collection and its navigation intact.
        let end_page = if page == 0 {
            self.collection_pages.max(1)
        } else {
            page + 1
        };
        let client = self.session.read(cx).client().clone();
        let user = self.session.read(cx).me().id.clone();
        self._search = Some(cx.spawn(async move |this, cx| {
            let result = async {
                let mut posts = Vec::new();
                let mut depth = page;
                let mut next = None;
                for page_ix in page..end_page {
                    let found = match &collection {
                        Collection::Saved => client.flagged_posts(&user, page_ix, 60).await?,
                        Collection::Pinned(channel) => client.pinned_posts(channel).await?,
                    };
                    let batch = found.into_chronological();
                    let has_more = collection == Collection::Saved && batch.len() == 60;
                    posts.extend(batch);
                    depth = page_ix + 1;
                    next = has_more.then_some(depth);
                    if !has_more {
                        break;
                    }
                }
                Ok::<_, anyhow::Error>((posts, depth, next))
            }
            .await;
            // Resolve channels outside the startup team before rows can open.
            if let Ok((posts, _, _)) = &result {
                let ids: std::collections::HashSet<_> =
                    posts.iter().map(|post| post.channel_id.clone()).collect();
                if let Ok(tasks) = this.update(cx, |this, cx| {
                    if revision != this.revision {
                        return Vec::new();
                    }
                    this.session.update(cx, |session, cx| {
                        ids.into_iter()
                            .map(|id| session.ensure_channel(&id, cx))
                            .collect::<Vec<_>>()
                    })
                }) {
                    futures::future::join_all(tasks).await;
                }
            }
            let _ = this.update(cx, |this, cx| {
                if revision != this.revision {
                    return;
                }
                this.loading_more = false;
                match result {
                    Ok((posts, depth, next)) => {
                        this.session
                            .read(cx)
                            .directory()
                            .clone()
                            .update(cx, |directory, cx| {
                                directory.ensure(posts.iter().map(|p| p.user_id.clone()), cx)
                            });
                        let mut rows = if page == 0 {
                            Vec::new()
                        } else {
                            this.results.clone()
                        };
                        for post in posts.into_iter().filter(|p| {
                            p.delete_at == 0
                                && this.session.read(cx).channel(&p.channel_id).is_some()
                        }) {
                            if !rows.iter().any(|p| p.id == post.id) {
                                rows.push(Arc::new(post));
                            }
                        }
                        rows.sort_by(|a, b| {
                            b.create_at.cmp(&a.create_at).then_with(|| a.id.cmp(&b.id))
                        });
                        this.replace_collection_rows(rows);
                        this.collection_pages = depth;
                        this.next_page = next;
                        this.results_scope = Some(SearchScope::Team);
                        this.status = Status::Done;
                    }
                    Err(_) if page > 0 => this.more_failed = true,
                    Err(err) => {
                        let permission = err.downcast_ref::<mattermost::ApiError>()
                            == Some(&mattermost::ApiError::Status(403));
                        this.status = Status::Failed(if permission {
                            "You don’t have permission to view these messages.".into()
                        } else {
                            "Couldn’t load messages. Check your connection and retry.".into()
                        });
                    }
                }
                this.prune_collection_channels(cx);
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn render_collection(&self, cx: &Context<Self>) -> AnyElement {
        let collection = self.collection.as_ref().unwrap();
        let title = collection.title();
        let body = match &self.status {
            Status::Failed(error) if self.results.is_empty() => v_flex()
                .p_4()
                .gap_3()
                .child(error.clone())
                .child(
                    Button::new("retry-collection")
                        .label("Retry")
                        .on_click(cx.listener(|this, _, _, cx| this.load_collection(0, cx))),
                )
                .into_any_element(),
            Status::Searching if self.results.is_empty() => v_flex()
                .p_4()
                .gap_3()
                .child(Spinner::new())
                .child("Loading messages…")
                .into_any_element(),
            _ if self.results.is_empty() => div()
                .p_4()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(collection.empty())
                .into_any_element(),
            _ => {
                let view = cx.entity().downgrade();
                gpui_kit::list(self.collection_scroll.clone(), move |ix, _, cx| {
                    let Some(view) = view.upgrade() else {
                        return div().into_any_element();
                    };
                    view.update(cx, |this, cx| {
                        this.render_result(ix, &this.results[ix], &[], cx)
                    })
                })
                .flex_1()
                .min_h_0()
                .into_any_element()
            }
        };
        v_flex()
            .id("message-collection")
            .size_full()
            .min_w_0()
            .child(
                h_flex()
                    .h_16()
                    .flex_none()
                    .px_4()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().font_semibold().child(title))
                    .when(
                        matches!(self.status, Status::Searching) && !self.results.is_empty(),
                        |this| this.child(Spinner::new().xsmall()),
                    )
                    .child(
                        Button::new("close-collection")
                            .small()
                            .ghost()
                            .icon(IconName::X)
                            .accessibility_label(format!("Close {title}"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(SearchEvent::Close))),
                    ),
            )
            .child(
                v_flex()
                    .id("collection-list")
                    .track_focus(&self.collection_focus)
                    .on_key_down(cx.listener(Self::on_key_down))
                    .focus_visible(|s| s.border_1().border_color(cx.theme().ring))
                    .flex_1()
                    .min_h_0()
                    .child(body),
            )
            .when(
                !self.results.is_empty() && matches!(self.status, Status::Failed(_)),
                |view| {
                    view.child(crate::recovery::inline_failure(
                        "Couldn’t refresh messages.",
                        Button::new("retry-collection-refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.load_collection(0, cx))),
                        cx,
                    ))
                },
            )
            .when(self.more_failed, |view| {
                view.child(crate::recovery::inline_failure(
                    "Couldn’t load more messages.",
                    Button::new("retry-collection-page")
                        .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                    cx,
                ))
            })
            .when(self.next_page.is_some() && !self.more_failed, |view| {
                view.child(
                    div().p_3().child(
                        Button::new("more-collection")
                            .small()
                            .ghost()
                            .w_full()
                            .label(if self.more_failed {
                                "Retry"
                            } else {
                                "Show more messages"
                            })
                            .loading(self.loading_more || matches!(self.status, Status::Searching))
                            .on_click(cx.listener(|this, _, _, cx| this.load_more(cx))),
                    ),
                )
            })
            .into_any_element()
    }
}
