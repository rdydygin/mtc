//! The message field.
//!
//! Enter sends, Shift-Enter adds a line, Up in an empty field edits your
//! last message, Escape dismisses suggestions or cancels an edit. Tab or Enter
//! accepts a visible suggestion; Enter sends when there are no suggestions.
//! The composer only reports intent; its owner decides what sending means.
//!
//! Files come in through the attach button, drag and drop, or pasting an
//! image. Each uploads at once, so sending is instant; a message can't be
//! sent while an upload is still running.

mod suggestions;

use suggestions::{Kind, Suggestion, Token, channel_matches, token_at, user_matches};

use gpui_kit::assets::IconName;
use std::{collections::HashSet, path::PathBuf, sync::Arc, time::Duration};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
    attachment::{
        Attachment, AttachmentContent, AttachmentDescription, AttachmentMedia, AttachmentStatus,
        AttachmentTitle,
    },
    avatar::Avatar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Enter, Escape, IndentInline, InputEvent, MoveDown, MoveUp, Textarea, TextareaState},
    list::ListItem,
    v_flex,
};
use gpui_kit::{
    AppContext as _, ClipboardEntry, ClipboardItem, Context, Entity, EntityInputHandler as _,
    EventEmitter, FocusHandle, Focusable, Image, ImageFormat, ImageSource, InteractiveElement as _,
    IntoElement, KeyDownEvent, ParentElement as _, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, Task, WeakEntity, Window, div,
    prelude::FluentBuilder as _,
};
use mattermost::{ChannelId, Client, FileInfo, TeamId, UserAutocomplete};
use session::Session;

use crate::extensions;

/// Where attached files are uploaded.
#[derive(Clone)]
pub struct UploadTarget {
    pub client: Client,
    pub channel: ChannelId,
}

/// The file's bytes, kept so a failed upload can be retried.
#[derive(Clone)]
enum FileSource {
    Path(PathBuf),
    Pasted(Arc<Image>),
}

struct PendingFile {
    local_id: u64,
    name: SharedString,
    size: u64,
    source: FileSource,
    /// The picture the attachment shows, if it is one: bytes the app holds,
    /// whose raster format was read from those same bytes. GPUI is never
    /// given a path to read again, which by then could hold an SVG it would
    /// render in the app (see `crate::svg_process`). Without one the
    /// attachment shows a file icon.
    preview: Option<Arc<Image>>,
    /// Reading the preview; dropped, and so stopped, with the file.
    _preview: Option<Task<()>>,
    status: AttachmentStatus,
    uploaded: Option<FileInfo>,
    error: Option<SharedString>,
    _upload: Option<Task<()>>,
}

pub enum ComposerEvent {
    /// The user sent the text (or saved an edit) with uploaded files.
    Submit { text: String, files: Vec<FileInfo> },
    /// Up was pressed in an empty field.
    EditLast,
    /// The user canceled editing.
    CancelEdit,
    /// The text changed; others may be told the user is typing.
    Typing,
    /// The unsent text changed, including when it becomes empty.
    DraftChanged,
}

pub struct Composer {
    input: Entity<TextareaState>,
    emoji_search: Entity<crate::emoji_picker::EmojiSearch>,
    editing: bool,
    saved_draft: Option<String>,
    focus_handle: FocusHandle,
    selected_suggestion: usize,
    selected_emoji: Option<String>,
    selected_command: Option<String>,
    dismissed: Option<Token>,
    token: Option<Token>,
    composing: bool,
    session: Option<Entity<Session>>,
    lookup_team: Option<TeamId>,
    lookup_generation: u64,
    remote_users: Vec<Arc<mattermost::User>>,
    _lookup: Option<Task<()>>,
    target: UploadTarget,
    files: Vec<PendingFile>,
    next_file_id: u64,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(
        placeholder: impl Into<SharedString>,
        draft: Option<String>,
        target: UploadTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let placeholder = placeholder.into();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 12)
                .submit_on_enter(true)
                .placeholder(placeholder)
        });
        if let Some(draft) = draft {
            input.update(cx, |state, cx| {
                state.set_value(draft.clone(), window, cx);
                state.set_selected_range(draft.len()..draft.len(), cx);
            });
        }
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => {
                        if !this.composing {
                            this.submit(window, cx);
                        }
                    }
                    InputEvent::Change => {
                        this.selected_suggestion = 0;
                        this.dismissed = None;
                        this.refresh_suggestions(window, cx);
                        if !input.read(cx).value().is_empty() {
                            cx.emit(ComposerEvent::Typing);
                        }
                        cx.emit(ComposerEvent::DraftChanged);
                        cx.notify();
                    }
                    _ => {}
                },
            );
        // Input observation also tracks caret/selection and IME changes, which
        // do not emit InputEvent::Change.
        let caret = cx.observe_in(&input, window, |this, _, window, cx| {
            this.refresh_suggestions(window, cx);
        });
        let store = cx.new(|_| session::EmojiStore::new(target.client.clone()));
        let emoji_search =
            cx.new(|cx| crate::emoji_picker::EmojiSearch::new(target.client.clone(), store, cx));
        let emoji_updates = cx.observe(&emoji_search, |this, _, cx| {
            this.reconcile_emoji_selection(cx)
        });
        let focus_handle = cx.focus_handle();
        let focus_in = cx.on_focus_in(&focus_handle, window, |_, _, cx| cx.notify());
        let focus_out = cx.on_focus_out(&focus_handle, window, |_, _, _, cx| cx.notify());
        Self {
            input,
            emoji_search,
            editing: false,
            saved_draft: None,
            focus_handle,
            selected_suggestion: 0,
            selected_emoji: None,
            selected_command: None,
            dismissed: None,
            token: None,
            composing: false,
            session: None,
            lookup_team: None,
            lookup_generation: 0,
            remote_users: Vec::new(),
            _lookup: None,
            target,
            files: Vec::new(),
            next_file_id: 0,
            _subscriptions: vec![subscription, caret, focus_in, focus_out, emoji_updates],
        }
    }

    fn is_uploading(&self) -> bool {
        self.files
            .iter()
            .any(|f| f.uploaded.is_none() && f.error.is_none())
    }

    fn can_submit(&self, cx: &gpui_kit::App) -> bool {
        let has_text = !self.input.read(cx).value().trim().is_empty();
        let has_files = self.files.iter().any(|f| f.uploaded.is_some());
        if self.editing {
            has_text
        } else {
            !self.is_uploading() && (has_text || has_files)
        }
    }

    /// Send the text and every uploaded file. Failed uploads stay behind so
    /// they can be retried or removed.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_submit(cx) {
            return;
        }
        let text = self.text(cx);
        let files = if self.editing {
            Vec::new()
        } else {
            let mut sent = Vec::new();
            self.files.retain_mut(|f| match f.uploaded.take() {
                Some(info) => {
                    sent.push(info);
                    false
                }
                None => true,
            });
            sent
        };
        if self.editing {
            self.set_editing(None, window, cx);
        } else {
            self.set_text("", window, cx);
        }
        cx.emit(ComposerEvent::Submit { text, files });
        cx.notify();
    }

    /// Ask for files with the system dialog.
    pub fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                let _ = this.update(cx, |this, cx| this.attach_paths(paths, cx));
            }
        })
        .detach();
    }

    /// Attach files dropped on the conversation or chosen in a dialog.
    pub fn attach_paths(
        &mut self,
        paths: impl IntoIterator<Item = PathBuf>,
        cx: &mut Context<Self>,
    ) {
        if self.editing {
            return;
        }
        for path in paths {
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".into());
            let wants_preview = image_extension(&name);
            let source = FileSource::Path(path.clone());
            let local_id = self.add_file(name, metadata.len(), source, None, cx);
            if wants_preview {
                self.load_preview(local_id, path, cx);
            }
        }
    }

    /// Read the picture at `path` once, off the UI thread, and show those
    /// bytes if they are a raster image.
    fn load_preview(&mut self, local_id: u64, path: PathBuf, cx: &mut Context<Self>) {
        let Some(file) = self.files.iter_mut().find(|f| f.local_id == local_id) else {
            return;
        };
        file._preview = Some(cx.spawn(async move |this, cx| {
            let preview = cx
                .background_spawn(async move { read_preview(&path) })
                .await;
            this.update(cx, |this, cx| {
                if let Some(file) = this.files.iter_mut().find(|f| f.local_id == local_id) {
                    file.preview = preview;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Attach images from a paste; returns whether the paste was handled.
    fn attach_pasted(&mut self, item: &ClipboardItem, cx: &mut Context<Self>) -> bool {
        if self.editing {
            return false;
        }
        let mut handled = false;
        for entry in item.entries() {
            match entry {
                ClipboardEntry::Image(image) => {
                    let name = format!("Pasted image.{}", format_extension(image.format));
                    let size = image.bytes.len() as u64;
                    let image = Arc::new(image.clone());
                    // GPUI decodes a pasted image by the format it claims.
                    let preview =
                        (raster_format(&image.bytes) == Some(image.format)).then(|| image.clone());
                    self.add_file(name, size, FileSource::Pasted(image), preview, cx);
                    handled = true;
                }
                ClipboardEntry::ExternalPaths(paths) => {
                    self.attach_paths(paths.paths().to_vec(), cx);
                    handled = true;
                }
                ClipboardEntry::String(_) => {}
            }
        }
        handled
    }

    fn add_file(
        &mut self,
        name: String,
        size: u64,
        source: FileSource,
        preview: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) -> u64 {
        let local_id = self.next_file_id;
        self.next_file_id += 1;
        self.files.push(PendingFile {
            local_id,
            name: name.into(),
            size,
            source,
            preview,
            _preview: None,
            status: AttachmentStatus::Uploading,
            uploaded: None,
            error: None,
            _upload: None,
        });
        self.upload(local_id, cx);
        local_id
    }

    fn upload(&mut self, local_id: u64, cx: &mut Context<Self>) {
        let Some(file) = self.files.iter_mut().find(|f| f.local_id == local_id) else {
            return;
        };
        file.status = AttachmentStatus::Uploading;
        file.error = None;
        let source = file.source.clone();
        let name = file.name.to_string();
        let target = self.target.clone();
        file._upload = Some(cx.spawn(async move |this, cx| {
            let bytes = match source {
                FileSource::Path(path) => cx
                    .background_spawn(async move { read_upload(&path) })
                    .await
                    .map_err(anyhow::Error::from),
                FileSource::Pasted(image) => Ok(image.bytes.clone()),
            };
            let result = match bytes {
                Ok(bytes) => {
                    target
                        .client
                        .upload_file(&target.channel, name, bytes)
                        .await
                }
                Err(err) => Err(err),
            };
            let _ = this.update(cx, |this, cx| {
                if let Some(file) = this.files.iter_mut().find(|f| f.local_id == local_id) {
                    match result {
                        Ok(info) => {
                            file.status = AttachmentStatus::Complete;
                            file.uploaded = Some(info);
                        }
                        Err(err) => {
                            file.status = AttachmentStatus::Failed;
                            log::warn!("uploading attachment: {err:#}");
                            file.error = Some("Couldn’t upload this file.".into());
                        }
                    }
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn remove_file(&mut self, local_id: u64, cx: &mut Context<Self>) {
        self.files.retain(|f| f.local_id != local_id);
        cx.notify();
    }

    fn render_files(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        if self.editing || self.files.is_empty() {
            return None;
        }
        let entity = cx.entity().downgrade();
        Some(
            h_flex()
                .flex_wrap()
                .gap_2()
                .px_2()
                .pt_2()
                .children(self.files.iter().map(|file| render_file(file, &entity, cx))),
        )
    }

    pub fn text(&self, cx: &gpui_kit::App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_suggestion = 0;
        self.selected_emoji = None;
        self.selected_command = None;
        self.dismissed = None;
        self.input.update(cx, |state, cx| {
            state.set_value(text.to_string(), window, cx);
            state.set_selected_range(text.len()..text.len(), cx);
        });
        // Programmatic set_value deliberately suppresses InputEvent::Change.
        // Callers establish editing/saved_draft before replacing the input.
        cx.emit(ComposerEvent::DraftChanged);
        cx.notify();
    }

    /// Enter edit mode with `text`, or leave it.
    pub fn set_editing(&mut self, text: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        match text {
            Some(text) => {
                if !self.editing {
                    self.saved_draft = Some(self.text(cx));
                }
                self.editing = true;
                self.set_text(text, window, cx);
            }
            None => {
                self.editing = false;
                if let Some(draft) = self.saved_draft.take() {
                    self.set_text(&draft, window, cx);
                }
            }
        }
        self.focus_handle(cx).focus(window, cx);
    }

    /// The unsent message, including while an older message is being edited.
    pub fn draft(&self, cx: &gpui_kit::App) -> String {
        self.saved_draft.clone().unwrap_or_else(|| self.text(cx))
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_editing(None, window, cx);
        cx.emit(ComposerEvent::CancelEdit);
    }

    /// Share the live directory and joined channels with this conversation.
    pub fn with_session(
        mut self,
        session: Entity<Session>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let directory = session.read(cx).directory().clone();
        self._subscriptions
            .push(cx.observe(&directory, |_, _, cx| cx.notify()));
        self._subscriptions
            .push(cx.observe_in(&session, window, |this, _, window, cx| {
                this.refresh_suggestions(window, cx);
                this.reconcile_command_selection(cx);
                cx.notify();
            }));
        let store = session.read(cx).emoji_store().clone();
        self.emoji_search = cx
            .new(|cx| crate::emoji_picker::EmojiSearch::new(self.target.client.clone(), store, cx));
        self._subscriptions
            .push(cx.observe(&self.emoji_search, |this, _, cx| {
                this.reconcile_emoji_selection(cx)
            }));
        self.session = Some(session);
        self.refresh_suggestions(window, cx);
        self
    }

    fn current_token(&self, cx: &gpui_kit::App) -> Option<Token> {
        let input = self.input.read(cx);
        if self.composing || !input.selected_range().is_empty() {
            return None;
        }
        let token = token_at(&input.value(), input.cursor())?;
        if token.kind == Kind::Command && self.editing {
            return None;
        }
        Some(token)
    }

    fn refresh_suggestions(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.composing = self.input.update(cx, |input, cx| {
            input.marked_text_range(window, cx).is_some()
        });
        let token = self.current_token(cx);
        let team = self
            .session
            .as_ref()
            .and_then(|s| s.read(cx).team().map(|t| t.id.clone()));
        if token == self.token && team == self.lookup_team {
            return;
        }
        self.lookup_generation += 1;
        self._lookup = None;
        self.remote_users.clear();
        self.selected_suggestion = 0;
        self.selected_emoji = None;
        self.selected_command = None;
        self.dismissed = None;
        self.token = token.clone();
        self.lookup_team = team.clone();
        cx.notify();
        self.emoji_search.update(cx, |search, cx| {
            search.set_query(
                token
                    .as_ref()
                    .filter(|t| t.kind == Kind::Emoji)
                    .map_or("", |t| t.query.as_str()),
                cx,
            )
        });
        if token.as_ref().is_some_and(|t| t.kind == Kind::Emoji) {
            self.selected_emoji = self
                .suggestion_matches(cx)
                .first()
                .map(|row| row.completion.clone());
        }
        self.reconcile_command_selection(cx);
        let (Some(token), Some(team)) = (token, team) else {
            return;
        };
        if token.kind != Kind::Person {
            return;
        }
        let client = self.target.client.clone();
        let channel = self.target.channel.clone();
        let generation = self.lookup_generation;
        self._lookup = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(180))
                .await;
            // Check again after debounce: the caret may have moved before its
            // observer runs. Requests already on the wire have the same guard.
            if !this
                .update(cx, |this, cx| {
                    this.lookup_is_current(generation, &token, cx)
                })
                .unwrap_or(false)
            {
                return;
            }
            if let Ok(users) = client
                .autocomplete_users(&team, &channel, &token.query)
                .await
            {
                let _ = this.update(cx, |this, cx| {
                    this.receive_users(generation, &token, users, cx)
                });
            }
        }));
    }

    fn lookup_is_current(&self, generation: u64, token: &Token, cx: &gpui_kit::App) -> bool {
        self.lookup_generation == generation
            && self.dismissed.as_ref() != Some(token)
            && self.current_token(cx).as_ref() == Some(token)
            && self
                .session
                .as_ref()
                .and_then(|s| s.read(cx).team().map(|t| &t.id))
                == self.lookup_team.as_ref()
    }

    fn receive_users(
        &mut self,
        generation: u64,
        token: &Token,
        users: UserAutocomplete,
        cx: &mut Context<Self>,
    ) {
        if !self.lookup_is_current(generation, token, cx) {
            return;
        }
        // Preserve the active person if server ranking inserts rows above it.
        let selected = self
            .suggestion_matches(cx)
            .get(self.selected_suggestion)
            .map(|s| s.id.clone());
        self.remote_users = users
            .users()
            .iter()
            .chain(users.out_of_channel())
            .cloned()
            .map(Arc::new)
            .collect();
        if let Some(session) = &self.session {
            let directory = session.read(cx).directory().clone();
            directory.update(cx, |d, cx| {
                d.insert(self.remote_users.iter().map(|u| u.as_ref().clone()), cx)
            });
        }
        if let Some(selected) = selected {
            self.selected_suggestion = self
                .suggestion_matches(cx)
                .iter()
                .position(|s| s.id == selected)
                .unwrap_or(0);
        }
        cx.notify();
    }

    fn reconcile_emoji_selection(&mut self, cx: &mut Context<Self>) {
        if self
            .current_token(cx)
            .is_some_and(|token| token.kind == Kind::Emoji)
        {
            let matches = self.suggestion_matches(cx);
            self.selected_emoji = matches
                .get(self.selected_index(&matches))
                .map(|row| row.completion.clone());
        }
        cx.notify();
    }

    fn reconcile_command_selection(&mut self, cx: &gpui_kit::App) {
        if self
            .current_token(cx)
            .is_some_and(|token| token.kind == Kind::Command)
        {
            let matches = self.suggestion_matches(cx);
            self.selected_command = matches
                .get(self.selected_index(&matches))
                .map(|row| row.completion.clone());
        }
    }

    fn selected_index(&self, matches: &[Suggestion]) -> usize {
        if let Some(name) = self
            .selected_command
            .as_ref()
            .or(self.selected_emoji.as_ref())
        {
            matches
                .iter()
                .position(|row| &row.completion == name)
                .unwrap_or(0)
        } else {
            self.selected_suggestion
                .min(matches.len().saturating_sub(1))
        }
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if !event.keystroke.modifiers.modified() {
            self.handle_key(&event.keystroke.key, window, cx);
        }
    }

    /// Input actions run before raw key events, so both paths share this handler.
    fn handle_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_suggestions(window, cx);
        if self.composing {
            cx.propagate();
            return;
        }
        let matches = self.suggestion_matches(cx);
        if !matches.is_empty() {
            self.selected_suggestion = self.selected_index(&matches);
            match key {
                "up" => {
                    self.selected_suggestion =
                        (self.selected_suggestion + matches.len() - 1) % matches.len()
                }
                "down" => self.selected_suggestion = (self.selected_suggestion + 1) % matches.len(),
                "tab" | "enter" => {
                    let completion =
                        &matches[self.selected_suggestion.min(matches.len() - 1)].completion;
                    self.complete_suggestion(completion, window, cx);
                }
                "escape" => {
                    self.dismissed = self.current_token(cx);
                    self.emoji_search.update(cx, |search, cx| search.cancel(cx));
                    self.lookup_generation += 1;
                    self._lookup = None;
                }
                _ => {
                    cx.propagate();
                    return;
                }
            }
            if matches!(key, "up" | "down")
                && self
                    .current_token(cx)
                    .is_some_and(|token| token.kind == Kind::Emoji)
            {
                self.selected_emoji = Some(matches[self.selected_suggestion].completion.clone());
            }
            if matches!(key, "up" | "down")
                && self
                    .current_token(cx)
                    .is_some_and(|token| token.kind == Kind::Command)
            {
                self.selected_command = Some(matches[self.selected_suggestion].completion.clone());
            }
            cx.notify();
            cx.stop_propagation();
            return;
        }
        // A lookup can have no local rows yet. Escape still dismisses that
        // pending surface, rather than letting its eventual response reopen it.
        if key == "escape"
            && (self._lookup.is_some() || self.emoji_search.read(cx).is_loading())
            && self.current_token(cx).is_some()
        {
            self.dismissed = self.current_token(cx);
            self.emoji_search.update(cx, |search, cx| search.cancel(cx));
            self.lookup_generation += 1;
            self._lookup = None;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        match key {
            "up" if self.input.read(cx).value().is_empty() => {
                cx.emit(ComposerEvent::EditLast);
                cx.stop_propagation();
            }
            "escape" if self.editing => {
                self.cancel_edit(window, cx);
                cx.stop_propagation();
            }
            _ => cx.propagate(),
        }
    }

    fn complete_suggestion(
        &mut self,
        completion: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(token) = self.current_token(cx) else {
            return;
        };
        // Use the input's edit operation to retain undo history and place the
        // caret after the inserted space. GPUI's input handler uses UTF-16.
        let text = self.text(cx);
        let range = text[..token.range.start].encode_utf16().count()
            ..text[..token.range.end].encode_utf16().count();
        self.input.update(cx, |state, cx| {
            state.replace_text_in_range(Some(range), &format!("{completion} "), window, cx);
        });
        self.refresh_suggestions(window, cx);
        self.focus_handle(cx).focus(window, cx);
    }

    fn suggestion_matches(&self, cx: &gpui_kit::App) -> Vec<Suggestion> {
        let Some(token) = self.current_token(cx) else {
            return Vec::new();
        };
        if self.dismissed.as_ref() == Some(&token) {
            return Vec::new();
        }
        match token.kind {
            Kind::Emoji => {
                session::emoji_matches(self.emoji_search.read(cx).custom(cx), &token.query, &[], 6)
                    .iter()
                    .map(|name| Suggestion {
                        id: format!("emoji-{name}"),
                        completion: format!(":{name}:"),
                        label: format!(":{name}:"),
                        detail: String::new(),
                        user: None,
                    })
                    .collect()
            }
            Kind::Command => {
                let extensions = extensions::slash_commands(cx);
                let mut seen: HashSet<_> =
                    extensions.iter().map(|c| c.name().to_string()).collect();
                let mut matches: Vec<_> = extensions
                    .iter()
                    .filter(|c| c.name().starts_with(&token.query))
                    .map(|c| Suggestion::command(c.name(), c.hint_text(), c.description()))
                    .collect();
                if let Some(session) = &self.session {
                    let session = session.read(cx);
                    let mut server: Vec<_> = session
                        .autocomplete_commands()
                        .iter()
                        .filter(|c| {
                            !c.trigger().is_empty() && c.trigger().starts_with(&token.query)
                        })
                        .collect();
                    server.sort_by_key(|c| c.trigger());
                    matches.extend(
                        server
                            .into_iter()
                            .filter(|c| seen.insert(c.trigger().to_string()))
                            .map(|c| Suggestion::command(c.trigger(), c.hint(), c.description())),
                    );
                }
                matches.truncate(6);
                matches
            }
            Kind::Channel => {
                let Some(session) = &self.session else {
                    return Vec::new();
                };
                let session = session.read(cx);
                let Some(team) = session.team() else {
                    return Vec::new();
                };
                channel_matches(
                    session.channels().map(|e| e.channel()),
                    &team.id,
                    &token.query,
                )
            }
            Kind::Person => {
                let mut local = self
                    .session
                    .as_ref()
                    .map(|s| {
                        s.read(cx)
                            .directory()
                            .read(cx)
                            .users()
                            .filter(|u| user_matches(u, &token.query))
                            .cloned()
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                local.sort_by_key(|u| (u.username.to_lowercase(), u.id.clone()));
                let mut seen = HashSet::new();
                let remote_current = self.token.as_ref() == Some(&token);
                let mut matches: Vec<_> = self
                    .remote_users
                    .iter()
                    .filter(|_| remote_current)
                    .chain(local.iter())
                    .filter(|u| user_matches(u, &token.query) && seen.insert(u.id.clone()))
                    .take(6)
                    .cloned()
                    .map(Suggestion::person)
                    .collect();
                for (name, description) in [
                    ("here", "Notify online members"),
                    ("channel", "Notify channel members"),
                    ("all", "Notify everyone"),
                ] {
                    if name.starts_with(&token.query) {
                        matches.push(Suggestion {
                            id: format!("mention-{name}"),
                            completion: format!("@{name}"),
                            label: format!("@{name}"),
                            detail: description.into(),
                            user: None,
                        });
                    }
                }
                matches
            }
        }
    }

    fn render_suggestions(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let matches = self.suggestion_matches(cx);
        if matches.is_empty() {
            return None;
        }
        let selected = self.selected_index(&matches);
        let theme = cx.theme();
        Some(
            v_flex()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .children(matches.into_iter().enumerate().map(|(ix, suggestion)| {
                    let completion = suggestion.completion.clone();
                    let avatar = suggestion.user.as_ref().map(|user| {
                        Avatar::new().name(suggestion.label.clone()).size_6().src(
                            crate::http::avatar_url(
                                &self.target.client,
                                user.id.as_str(),
                                user.last_picture_update,
                            ),
                        )
                    });
                    ListItem::new(SharedString::from(suggestion.id))
                        .selected(ix == selected)
                        .aria_selected(ix == selected)
                        .accessibility_label(format!("{}: {}", suggestion.label, suggestion.detail))
                        .text_sm()
                        .child(
                            h_flex()
                                .gap_2()
                                .min_w_0()
                                .children(avatar)
                                .when(suggestion.completion.starts_with(':'), |row| {
                                    row.child(crate::status::emoji_element(
                                        suggestion.completion.trim_matches(':'),
                                        self.emoji_search.read(cx).custom(cx),
                                    ))
                                })
                                .child(
                                    div()
                                        .min_w_0()
                                        .truncate()
                                        .font_semibold()
                                        .child(suggestion.label),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_color(theme.muted_foreground)
                                        .child(suggestion.detail),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.complete_suggestion(&completion, window, cx)
                        }))
                })),
        )
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, cx: &gpui_kit::App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let suggestions = self
            .render_suggestions(cx)
            .map(IntoElement::into_any_element);
        let files = self.render_files(cx).map(IntoElement::into_any_element);
        let can_submit = self.can_submit(cx);
        let uploading = !self.editing && self.is_uploading();
        let editing = self.editing;
        let entity = cx.entity().downgrade();
        let theme = cx.theme();
        // One filled field, like the composers people already know: the
        // attach and send actions sit inside it, beside the text.
        div().px_4().pb_4().child(
            v_flex()
                .id("composer")
                .track_focus(&self.focus_handle)
                .border_1()
                .border_color(theme.secondary)
                .when(self.focus_handle.contains_focused(window, cx), |this| {
                    this.border_color(theme.ring)
                })
                .capture_key_down(cx.listener(Self::on_key_down))
                .capture_action(cx.listener(|this, action: &Enter, window, cx| {
                    if action.shift {
                        cx.propagate();
                    } else {
                        this.handle_key("enter", window, cx);
                    }
                }))
                .capture_action(
                    cx.listener(|this, _: &MoveUp, window, cx| this.handle_key("up", window, cx)),
                )
                .capture_action(
                    cx.listener(|this, _: &MoveDown, window, cx| {
                        this.handle_key("down", window, cx)
                    }),
                )
                .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                    this.handle_key("tab", window, cx)
                }))
                .capture_action(
                    cx.listener(|this, _: &Escape, window, cx| {
                        this.handle_key("escape", window, cx)
                    }),
                )
                .capture_action(
                    cx.listener(|this, _: &crate::actions::Dismiss, window, cx| {
                        this.handle_key("escape", window, cx)
                    }),
                )
                .rounded(theme.radius_lg)
                .bg(theme.secondary)
                .overflow_hidden()
                .when(editing, |this| {
                    this.child(
                        h_flex()
                            .px_3()
                            .pt_2()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(IconName::Pencil)
                            .child(div().flex_1().child("Editing message"))
                            .child(
                                Button::new("cancel-edit")
                                    .xsmall()
                                    .ghost()
                                    .label("Cancel")
                                    .tooltip("Cancel edit (Escape)")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_edit(window, cx)
                                    })),
                            ),
                    )
                })
                .children(suggestions)
                .children(files)
                .child(
                    // Buttons sit in lanes one line tall, aligned to the
                    // bottom, so they center on a single line and stay with
                    // the last line as the text grows.
                    h_flex()
                        .items_end()
                        .gap_1()
                        .px_1p5()
                        .when(!editing, |this| {
                            this.child(
                                div().h_11().flex().items_center().child(
                                    Button::new("attach")
                                        .small()
                                        .ghost()
                                        .rounded_full()
                                        .icon(IconName::Paperclip)
                                        .accessibility_label("Attach files")
                                        .tooltip("Attach files")
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.pick_files(window, cx)
                                        })),
                                ),
                            )
                        })
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .min_h_11()
                                .py_1()
                                .flex()
                                .items_center()
                                .child(
                                    Textarea::new(&self.input)
                                        .appearance(false)
                                        .aria_label("Message")
                                        .on_paste(move |item, _, cx| {
                                            entity
                                                .update(cx, |this, cx| this.attach_pasted(item, cx))
                                                .unwrap_or(false)
                                        }),
                                ),
                        )
                        .child(
                            div().h_11().flex().items_center().child(
                                Button::new("send")
                                    .small()
                                    .rounded_full()
                                    .map(|button| {
                                        if can_submit {
                                            button.primary()
                                        } else {
                                            button.ghost()
                                        }
                                    })
                                    .icon(IconName::ArrowUp)
                                    // The name is the action; while uploads run
                                    // the button is disabled and the tooltip,
                                    // exposed as its description, says why.
                                    .accessibility_label(if editing {
                                        "Save edit"
                                    } else {
                                        "Send message"
                                    })
                                    .tooltip(if uploading {
                                        "Waiting for uploads"
                                    } else if editing {
                                        "Save"
                                    } else {
                                        "Send"
                                    })
                                    .disabled(!can_submit)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.submit(window, cx)),
                                    ),
                            ),
                        ),
                ),
        )
    }
}

fn render_file(
    file: &PendingFile,
    entity: &WeakEntity<Composer>,
    cx: &gpui_kit::App,
) -> impl IntoElement + use<> {
    let local_id = file.local_id;
    let media = match &file.preview {
        Some(image) => AttachmentMedia::new().src(ImageSource::Image(image.clone())),
        None => AttachmentMedia::new().child(Icon::new(IconName::File)),
    };
    let description: SharedString = format_size(file.size).into();
    let remove = entity.clone();
    let retry = entity.clone();
    v_flex()
        .child(
            Attachment::new()
                .id(SharedString::from(format!("upload-{local_id}")))
                .status(file.status)
                .media(media)
                .content(
                    AttachmentContent::new()
                        .title(AttachmentTitle::new(file.name.clone()))
                        .description(AttachmentDescription::new(description)),
                )
                .tooltip(file.name.clone())
                .on_remove(move |_, _, cx| {
                    let _ = remove.update(cx, |this, cx| this.remove_file(local_id, cx));
                }),
        )
        .when_some(file.error.clone(), |this, message| {
            this.child(crate::recovery::inline_failure(
                message,
                Button::new(SharedString::from(format!("retry-upload-{local_id}"))).on_click(
                    move |_, _, cx| {
                        let _ = retry.update(cx, |this, cx| this.upload(local_id, cx));
                    },
                ),
                cx,
            ))
        })
}

fn image_extension(name: &str) -> bool {
    let lower = name.to_lowercase();
    [
        ".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp", ".heic", ".tif", ".tiff",
    ]
    .iter()
    .any(|ext| lower.ends_with(ext))
}

/// The most bytes read to preview a picked file: as many as a picture
/// from the server may have.
const MAX_PREVIEW_BYTES: usize = crate::svg_process::MAX_INPUT;

/// `path` opened for reading if it is, by then, a regular file. A picked
/// file can since have been replaced by one that never ends or never
/// answers; opening one doesn't wait, and it is refused before any read.
///
/// On Unix opening a FIFO waits for a writer unless it is non-blocking,
/// which changes nothing for a regular file. On Windows a pipe reached
/// through a link opens at once, or fails, and std's `is_file` accepts it,
/// so the handle must be a disk file.
fn open_regular_file(path: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    #[cfg(windows)]
    let regular = {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::Storage::FileSystem::{FILE_TYPE_DISK, GetFileType};
        // SAFETY: the handle is open for as long as `file` lives.
        unsafe { GetFileType(file.as_raw_handle()) == FILE_TYPE_DISK }
    };
    #[cfg(not(windows))]
    let regular = file.metadata()?.is_file();
    if !regular {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Ok(file)
}

/// The whole file at `path`, to upload.
fn read_upload(path: &std::path::Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    open_regular_file(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The file at `path`, read once and held, if it is a raster image of at
/// most [`MAX_PREVIEW_BYTES`].
fn read_preview(path: &std::path::Path) -> Option<Arc<Image>> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    open_regular_file(path)
        .ok()?
        .take(MAX_PREVIEW_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_PREVIEW_BYTES {
        return None;
    }
    let format = raster_format(&bytes)?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

/// The raster format `bytes` are in, if GPUI decodes it. GPUI renders any
/// other picture as SVG, in the app, whatever its name or claimed format.
///
/// GPUI decodes WebP from bytes as a still image: an animated one shows its
/// first frame.
fn raster_format(bytes: &[u8]) -> Option<ImageFormat> {
    use image::ImageFormat as Raster;
    Some(match image::guess_format(bytes).ok()? {
        Raster::Png => ImageFormat::Png,
        Raster::Jpeg => ImageFormat::Jpeg,
        Raster::WebP => ImageFormat::Webp,
        Raster::Gif => ImageFormat::Gif,
        Raster::Bmp => ImageFormat::Bmp,
        Raster::Tiff => ImageFormat::Tiff,
        Raster::Ico => ImageFormat::Ico,
        Raster::Pnm => ImageFormat::Pnm,
        _ => return None,
    })
}

fn format_extension(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "jpg",
        ImageFormat::Webp => "webp",
        ImageFormat::Gif => "gif",
        ImageFormat::Svg => "svg",
        ImageFormat::Bmp => "bmp",
        ImageFormat::Tiff => "tiff",
        _ => "png",
    }
}

/// `2.4 MB`, `812 KB`, `90 bytes`.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.;
    let b = bytes as f64;
    if b >= KB * KB * KB {
        format!("{:.1} GB", b / (KB * KB * KB))
    } else if b >= KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} bytes")
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{AnyWindowHandle, TestAppContext, WindowOptions};

    use mattermost::testing::{Network, Reply, Request, SlowServer};
    use serde_json::json;
    use std::time::Instant;

    use super::*;

    /// Gradients that inherit from each other in a cycle: usvg, GPUI's SVG
    /// renderer, follows them forever.
    const HANG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><linearGradient id="a" xlink:href="#b"/><linearGradient id="b" xlink:href="#c"/><linearGradient id="c" xlink:href="#b"/><rect width="9" height="9" fill="url(#a)"/></svg>"##;

    fn png() -> Vec<u8> {
        use image::ImageEncoder as _;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255; 16], 2, 2, image::ExtendedColorType::Rgba8)
            .unwrap();
        png
    }

    fn scratch_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!("mtc-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn previews_only_bytes_that_are_a_raster_image() {
        let directory = scratch_directory("preview-bytes");
        let disguised = directory.join("disguised.png");
        std::fs::write(&disguised, HANG).unwrap();
        let picture = directory.join("picture.png");
        std::fs::write(&picture, png()).unwrap();
        assert!(read_preview(&disguised).is_none());
        let preview = read_preview(&picture).unwrap();
        assert_eq!(
            (preview.format(), preview.bytes()),
            (ImageFormat::Png, &png()[..])
        );
        assert!(read_preview(&directory.join("missing.png")).is_none());
        assert_eq!(raster_format(HANG), None);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A picked picture is read once; replacing the file afterwards, here
    /// with an SVG that would hang GPUI's renderer, changes nothing GPUI
    /// draws, and GPUI never reads the path itself.
    #[gpui_kit::test]
    fn a_picked_picture_shows_the_bytes_it_was_admitted_with(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        // Attaching also starts an upload, which fails on the I/O runtime.
        cx.executor().allow_parking();
        let directory = scratch_directory("preview-swap");
        let path = directory.join("picture.png");
        std::fs::write(&path, png()).unwrap();
        composer.update(cx, |c, cx| c.attach_paths([path.clone()], cx));
        wait_until(cx, |cx| {
            composer.read_with(cx, |c, _| c.files[0].preview.is_some())
        });

        std::fs::write(&path, HANG).unwrap();
        let redraw = |cx: &mut TestAppContext| {
            cx.update(|cx| handle.update(cx, |_, window, _| window.refresh()).unwrap());
            cx.run_until_parked();
        };
        redraw(cx);
        let preview = composer.read_with(cx, |c, _| c.files[0].preview.clone().unwrap());
        assert_eq!(
            (preview.format(), preview.bytes()),
            (ImageFormat::Png, &png()[..])
        );
        cx.update(|cx| {
            assert!(preview.is_asset_cached(cx), "GPUI draws the bytes read");
            assert!(!ImageSource::from(path.clone()).is_asset_cached(cx));
        });

        // Picked again, the file is read as the SVG it now is: no preview.
        composer.update(cx, |c, cx| c.attach_paths([path.clone()], cx));
        redraw(cx);
        composer.read_with(cx, |c, _| assert!(c.files[1].preview.is_none()));
        cx.update(|cx| assert!(!ImageSource::from(path.clone()).is_asset_cached(cx)));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn previews_files_up_to_the_size_limit() {
        let directory = scratch_directory("preview-limit");
        let mut bytes = png();
        bytes.resize(MAX_PREVIEW_BYTES, 0);
        let largest = directory.join("largest.png");
        std::fs::write(&largest, &bytes).unwrap();
        bytes.push(0);
        let larger = directory.join("larger.png");
        std::fs::write(&larger, &bytes).unwrap();
        assert_eq!(
            read_preview(&largest).unwrap().bytes().len(),
            MAX_PREVIEW_BYTES
        );
        assert!(read_preview(&larger).is_none());
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn raster_formats_are_the_ones_gpui_decodes() {
        assert_eq!(raster_format(&png()), Some(ImageFormat::Png));
        assert_eq!(raster_format(ICO), Some(ImageFormat::Ico));
        assert_eq!(
            raster_format(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(raster_format(b"GIF89a"), Some(ImageFormat::Gif));
        assert_eq!(raster_format(HANG), None);
        assert_eq!(raster_format(b""), None);
    }

    /// The start of an icon file: one 1 × 1 picture.
    const ICO: &[u8] = b"\0\0\x01\0\x01\0\x01\x01\0\0\x01\0\x20\0";

    #[gpui_kit::test]
    fn a_pasted_picture_previews_only_if_it_is_what_it_claims(cx: &mut TestAppContext) {
        let (_, composer) = open_composer(cx);
        // Each paste also starts an upload, which fails on the I/O runtime.
        cx.executor().allow_parking();
        let pastes = [
            (ImageFormat::Png, png(), true),
            (ImageFormat::Ico, ICO.to_vec(), true),
            (ImageFormat::Png, HANG.to_vec(), false),
            (ImageFormat::Svg, HANG.to_vec(), false),
            (ImageFormat::Jpeg, png(), false),
        ];
        for (format, bytes, _) in &pastes {
            let item = ClipboardItem::new_image(&Image::from_bytes(*format, bytes.clone()));
            composer.update(cx, |c, cx| assert!(c.attach_pasted(&item, cx)));
        }
        composer.read_with(cx, |c, _| {
            let shown: Vec<_> = c.files.iter().map(|f| f.preview.is_some()).collect();
            let expected: Vec<_> = pastes.iter().map(|(_, _, shown)| *shown).collect();
            assert_eq!(shown, expected);
        });
    }

    #[gpui_kit::test]
    fn removing_a_file_stops_its_preview(cx: &mut TestAppContext) {
        let (_, composer) = open_composer(cx);
        cx.executor().allow_parking();
        let directory = scratch_directory("preview-removed");
        let path = directory.join("picture.png");
        std::fs::write(&path, png()).unwrap();
        // The read is still to run when the file goes.
        composer.update(cx, |c, cx| {
            c.attach_paths([path.clone()], cx);
            assert!(c.files[0]._preview.is_some());
            let local_id = c.files[0].local_id;
            c.remove_file(local_id, cx);
        });
        cx.run_until_parked();
        composer.read_with(cx, |c, _| assert!(c.files.is_empty()));
        // Later files still get theirs.
        composer.update(cx, |c, cx| c.attach_paths([path.clone()], cx));
        wait_until(cx, |cx| {
            composer.read_with(cx, |c, _| c.files[0].preview.is_some())
        });
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[cfg(unix)]
    fn make_fifo(path: &std::path::Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: `path` is a valid C string for the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }

    /// Opening a FIFO for reading waits for a writer that never comes,
    /// unless it doesn't wait.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_waiting_for_it() {
        let directory = scratch_directory("preview-fifo");
        let fifo = directory.join("picture.png");
        make_fifo(&fifo);
        let (done, results) = std::sync::mpsc::channel();
        let path = fifo.clone();
        std::thread::spawn(move || {
            let _ = done.send((read_preview(&path).is_none(), read_upload(&path).is_err()));
        });
        let refused = results.recv_timeout(Duration::from_secs(20));
        assert_eq!(refused, Ok((true, true)), "a read waited on the FIFO");
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// A picked file replaced by a FIFO before it is read: the preview and
    /// the upload both give up at once.
    #[cfg(unix)]
    #[gpui_kit::test]
    fn a_file_swapped_for_a_fifo_fails_at_once(cx: &mut TestAppContext) {
        let (_, composer) = open_composer(cx);
        let directory = scratch_directory("preview-fifo-swap");
        let path = directory.join("picture.png");
        std::fs::write(&path, png()).unwrap();
        composer.update(cx, |c, cx| c.attach_paths([path.clone()], cx));
        std::fs::remove_file(&path).unwrap();
        make_fifo(&path);
        cx.run_until_parked();
        composer.read_with(cx, |c, _| {
            let file = &c.files[0];
            assert!(file.preview.is_none());
            assert_eq!(file.status, AttachmentStatus::Failed);
        });
        std::fs::remove_dir_all(&directory).unwrap();
    }

    fn wait_until(cx: &mut TestAppContext, mut done: impl FnMut(&mut TestAppContext) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting");
            cx.executor().advance_clock(Duration::from_millis(50));
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn route(request: &Request) -> Reply {
        match (
            request.method(),
            request.path().split('?').next().unwrap_or_default(),
        ) {
            ("GET", "/api/v4/users/me/teams") => Reply::json(json!([{"id":"team", "name":"team"}])),
            ("GET", "/api/v4/users/me/teams/team/channels") => Reply::json(json!([
                {"id":"town", "team_id":"team", "name":"town-square", "display_name":"Town Square"},
                {"id":"talk", "team_id":"team", "name":"town-talk", "display_name":"Town Talk"}
            ])),
            ("GET", "/api/v4/users/autocomplete") => Reply::json(json!({
                "users":[{"id":"ann", "username":"anna", "first_name":"Anna", "last_name":"Smith"}],
                "out_of_channel":[{"id":"amy", "username":"amy", "first_name":"Amy"}]
            })),
            (
                "GET",
                "/api/v4/users/me/teams/team/channels/members"
                | "/api/v4/users/me/preferences/favorite_channel"
                | "/api/v4/emoji",
            )
            | ("POST", "/api/v4/users/status/ids") => Reply::json(json!([])),
            _ => Reply::not_found(),
        }
    }

    fn open_composer(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<Composer>) {
        open_connected_composer(None, cx)
    }

    fn open_connected_composer(
        server: Option<&SlowServer>,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Composer>) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            extensions::init(cx);
        });
        let session = server.map(|server| {
            cx.executor().allow_parking();
            let client = Client::new_unverified(&server.url(), "token").unwrap();
            let me = serde_json::from_value(json!({"id":"me", "username":"zoe"})).unwrap();
            let session = cx.new(|cx| Session::new(client, me, Some("team".into()), cx));
            wait_until(cx, |cx| {
                session.read_with(cx, |s, _| s.is_loaded() && s.team().is_some())
            });
            session
        });
        let composer = Rc::new(RefCell::new(None));
        let (handle, _) = cx.update(|cx| {
            let composer = composer.clone();
            gpui_kit::open_window(WindowOptions::default(), cx, move |window, cx| {
                let view = cx.new(|cx| {
                    let target = UploadTarget {
                        client: session
                            .as_ref()
                            .map(|s| s.read(cx).client().clone())
                            .unwrap_or_else(|| {
                                Client::new_unverified("http://localhost:1", "token").unwrap()
                            }),
                        channel: "town".into(),
                    };
                    let composer =
                        Composer::new("Message", Some("Unsent draft".into()), target, window, cx);
                    match session {
                        Some(session) => composer.with_session(session, window, cx),
                        None => composer,
                    }
                });
                view.focus_handle(cx).focus(window, cx);
                *composer.borrow_mut() = Some(view.clone());
                view
            })
            .unwrap()
        });
        let composer = composer.borrow_mut().take().unwrap();
        cx.run_until_parked();
        (handle, composer)
    }

    #[gpui_kit::test]
    fn emoji_completion_uses_caret_token_and_never_sends_on_accept(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let submitted = submitted.clone();
            cx.subscribe(&composer, move |_, event, _| {
                if let ComposerEvent::Submit { text, .. } = event {
                    submitted.borrow_mut().push(text.clone());
                }
            })
        });
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| {
                c.set_text("🙂 (:smile:) after", window, cx);
                c.input
                    .update(cx, |input, cx| input.set_selected_range(11..11, cx));
            });
            window.render_frame(cx);
            assert_eq!(window.find("emoji-smile").selected(), Some(true));
            window.press("enter", cx);
            assert_eq!(composer.read(cx).text(cx), "🙂 (:smile: ) after");
            composer.update(cx, |c, cx| c.set_text(":he", window, cx));
            window.render_frame(cx);
            window.press("escape", cx);
            assert!(composer.read(cx).suggestion_matches(cx).is_empty());
            assert_eq!(composer.read(cx).text(cx), ":he");
            composer.update(cx, |c, cx| c.set_text(":zzzz_unknown", window, cx));
            window.press("escape", cx);
            let composer = composer.read(cx);
            assert_eq!(composer.dismissed, composer.current_token(cx));
            assert!(!composer.emoji_search.read(cx).is_loading());
        })
        .unwrap();
        cx.run_until_parked();
        assert!(
            submitted.borrow().is_empty(),
            "accepting an emoji must not submit"
        );
    }

    #[gpui_kit::test]
    fn delayed_custom_emoji_keeps_the_composer_selection(cx: &mut TestAppContext) {
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let server = SlowServer::start(Network::new(Duration::ZERO), move |r| {
            if r.method() == "POST" && r.path() == "/api/v4/emoji/search" {
                wait.lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
                Reply::json(json!([{"id":"remote", "name":"party_parrot"}]))
            } else {
                route(r)
            }
        });
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        let store = composer.read_with(cx, |c, cx| {
            c.session.as_ref().unwrap().read(cx).emoji_store().clone()
        });
        store.update(cx, |s, cx| {
            s.add(
                vec![serde_json::from_value(json!({"id":"local", "name":"party_popper"})).unwrap()],
                cx,
            )
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text(":party", window, cx));
        })
        .unwrap();
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .any(|r| r.method() == "POST" && r.path() == "/api/v4/emoji/search")
        });
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("emoji-party_popper").selected(), Some(true));
        })
        .unwrap();
        release.send(()).unwrap();
        wait_until(cx, |cx| {
            store.read_with(cx, |s, _| s.custom().contains("party_parrot"))
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(window.find("emoji-party_popper").selected(), Some(true));
            assert_eq!(window.find("emoji-party_parrot").selected(), Some(false));
            window.press("enter", cx);
            assert_eq!(composer.read(cx).text(cx), ":party_popper: ");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn mentions_replace_only_the_caret_token_and_enter_then_sends(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        let sent = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let sent = sent.clone();
            cx.subscribe(&composer, move |_, event, _| {
                if let ComposerEvent::Submit { text, .. } = event {
                    sent.borrow_mut().push(text.clone());
                }
            })
        });
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| {
                c.set_text("🙂 say (@he), next", window, cx);
                c.input
                    .update(cx, |input, cx| input.set_selected_range(13..13, cx));
            });
            window.render_frame(cx);
            assert_eq!(window.find("mention-here").selected(), Some(true));
            window.press("enter", cx);
            assert_eq!(composer.read(cx).text(cx), "🙂 say (@here ), next");
            assert_eq!(composer.read(cx).input.read(cx).cursor(), 16);
            assert!(sent.borrow().is_empty());
            window.input("now", cx);
            assert_eq!(composer.read(cx).text(cx), "🙂 say (@here now), next");
            window.press("enter", cx);
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(&*sent.borrow(), &["🙂 say (@here now), next"]);
    }

    #[gpui_kit::test]
    fn mentions_and_channels_share_keyboard_selection_and_dismissal(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("~town", window, cx));
            window.render_frame(cx);
            assert_eq!(window.find("channel-town").selected(), Some(true));
            window.press("down", cx);
            assert_eq!(window.find("channel-talk").selected(), Some(true));
            window.press("tab", cx);
            assert_eq!(composer.read(cx).text(cx), "~town-talk ");
            composer.update(cx, |c, cx| c.set_text("@", window, cx));
            window.press("up", cx);
            assert_eq!(window.find("mention-all").selected(), Some(true));
            window.press("escape", cx);
            assert_eq!(composer.read(cx).text(cx), "@");
            assert!(composer.read(cx).suggestion_matches(cx).is_empty());
            assert!(composer.focus_handle(cx).is_focused(window));
            window.input("he", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.press("tab", cx);
            assert_eq!(composer.read(cx).text(cx), "@here ");
            composer.update(cx, |c, cx| c.set_text("@he", window, cx));
            window.press("shift-enter", cx);
            assert_eq!(composer.read(cx).text(cx), "@he\n");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn autocomplete_is_debounced_ranked_and_rejects_stale_responses(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        let searches = || {
            server
                .requests()
                .into_iter()
                .filter(|r| {
                    r.method() == "GET" && r.path().starts_with("/api/v4/users/autocomplete?")
                })
                .collect::<Vec<_>>()
        };
        // A local result is usable without waiting for the server.
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| {
                let directory = c.session.as_ref().unwrap().read(cx).directory().clone();
                directory.update(cx, |d, cx| {
                    d.insert(
                        [
                            serde_json::from_value(json!({"id":"alice", "username":"alice"}))
                                .unwrap(),
                        ],
                        cx,
                    )
                });
                c.set_text("", window, cx);
            });
            window.input("@", cx);
            window.input("a", cx);
            assert_eq!(
                composer.read(cx).suggestion_matches(cx)[0].completion,
                "@alice"
            );
        })
        .unwrap();
        cx.run_until_parked();
        assert!(searches().is_empty());
        wait_until(cx, |cx| {
            composer.read_with(cx, |c, _| c.remote_users.len() == 2)
        });
        assert_eq!(searches().len(), 1);
        let requests = searches();
        let path = requests
            .iter()
            .find(|r| r.path().contains("name=a"))
            .unwrap()
            .path()
            .to_string();
        assert!(
            path.contains("in_team=team")
                && path.contains("in_channel=town")
                && path.contains("name=a"),
            "{path}"
        );
        composer.read_with(cx, |c, cx| {
            assert_eq!(
                c.suggestion_matches(cx)
                    .iter()
                    .map(|s| s.completion.as_str())
                    .collect::<Vec<_>>(),
                ["@anna", "@amy", "@alice", "@all"]
            );
        });
        assert_eq!(
            composer.read_with(cx, |c, _| c.selected_suggestion),
            2,
            "preserve Alice when ranking inserts other users above her"
        );
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| {
                let generation = c.lookup_generation;
                let token = c.current_token(cx).unwrap();
                // Even returning to exactly the same query must not accept an
                // earlier generation (the A → B → A race).
                c.set_text("@b", window, cx);
                c.refresh_suggestions(window, cx);
                c.set_text("@a", window, cx);
                c.refresh_suggestions(window, cx);
                let stale =
                    serde_json::from_value(json!({"users":[{"id":"wrong", "username":"a_wrong"}]}))
                        .unwrap();
                c.receive_users(generation, &token, stale, cx);
                assert!(c.remote_users.is_empty());
                // A response arriving after a caret move is rejected even if
                // the input observer has not run yet.
                let generation = c.lookup_generation;
                c.input.update(cx, |s, cx| s.set_selected_range(0..0, cx));
                c.receive_users(
                    generation,
                    &token,
                    serde_json::from_value(json!({"users":[{"id":"wrong"}]})).unwrap(),
                    cx,
                );
                assert!(c.remote_users.is_empty());
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn moving_the_caret_opens_suggestions_and_escape_cancels_pending_lookup(
        cx: &mut TestAppContext,
    ) {
        let server = SlowServer::start(Network::new(Duration::ZERO), route);
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("@he next", window, cx));
            assert!(composer.read(cx).suggestion_matches(cx).is_empty());
            for _ in 0..5 {
                window.press("left", cx);
            }
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(window.find("mention-here").selected(), Some(true));
            window.press("tab", cx);
            assert_eq!(composer.read(cx).text(cx), "@here  next");
            composer.update(cx, |c, cx| c.set_text("@zz", window, cx));
            composer.update(cx, |c, cx| c.refresh_suggestions(window, cx));
            assert!(composer.read(cx)._lookup.is_some());
            assert!(composer.read(cx).suggestion_matches(cx).is_empty());
            window.press("escape", cx);
            composer.read_with(cx, |c, cx| {
                assert!(c._lookup.is_none());
                assert!(c.dismissed.is_some());
                assert_eq!(c.text(cx), "@zz");
            });
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn url_suffixes_send_without_accepting_a_suggestion(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        let sent = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let sent = sent.clone();
            cx.subscribe(&composer, move |_, event, _| {
                if let ComposerEvent::Submit { text, .. } = event {
                    sent.borrow_mut().push(text.clone());
                }
            })
        });
        let urls = [
            "https://example.com/?@he",
            "https://example.com/#@he",
            "www.example.com/?@he",
        ];
        for url in urls {
            cx.update_window(handle, |_, window, cx| {
                composer.update(cx, |c, cx| c.set_text(url, window, cx));
                assert!(composer.read(cx).suggestion_matches(cx).is_empty());
                window.press("enter", cx);
            })
            .unwrap();
            cx.run_until_parked();
            composer.read_with(cx, |c, cx| assert!(c.text(cx).is_empty()));
        }
        assert_eq!(&*sent.borrow(), &urls);
    }

    #[gpui_kit::test]
    fn dismissing_or_replacing_a_lookup_closes_its_stalled_connection(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.method() == "GET"
                && request.path().split('?').next() == Some("/api/v4/users/autocomplete")
            {
                Reply::json(json!({"users": [], "out_of_channel": []})).stall_after(1)
            } else {
                route(request)
            }
        });
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        let matches = |request: &Request, name: &str| {
            request.method() == "GET"
                && request.path().split('?').next() == Some("/api/v4/users/autocomplete")
                && request.path().ends_with(&format!("&name={name}"))
        };
        let set_query = |text: &str, cx: &mut TestAppContext| {
            cx.update_window(handle, |_, window, cx| {
                composer.update(cx, |c, cx| c.set_text(text, window, cx));
            })
            .unwrap();
        };

        // No local rows: Escape must cancel the pending HTTP operation too.
        set_query("@zz", cx);
        wait_until(cx, |_| server.requests().iter().any(|r| matches(r, "zz")));
        assert!(
            !server
                .closed_stalled_requests()
                .iter()
                .any(|r| matches(r, "zz"))
        );
        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
            .unwrap();
        wait_until(cx, |_| {
            server
                .closed_stalled_requests()
                .iter()
                .any(|r| matches(r, "zz"))
        });

        // A newer query must abort the old request while starting its own.
        set_query("@a", cx);
        wait_until(cx, |_| server.requests().iter().any(|r| matches(r, "a")));
        set_query("@b", cx);
        wait_until(cx, |_| {
            server
                .closed_stalled_requests()
                .iter()
                .any(|r| matches(r, "a"))
        });
        wait_until(cx, |_| server.requests().iter().any(|r| matches(r, "b")));
        assert!(
            !server
                .closed_stalled_requests()
                .iter()
                .any(|r| matches(r, "b"))
        );
        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
            .unwrap();
        wait_until(cx, |_| {
            server
                .closed_stalled_requests()
                .iter()
                .any(|r| matches(r, "b"))
        });
    }

    #[gpui_kit::test]
    fn composing_and_selected_text_do_not_offer_or_accept_suggestions(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| {
                c.set_text("@he", window, cx);
                c.input.update(cx, |s, cx| s.set_selected_range(1..3, cx));
                assert!(c.suggestion_matches(cx).is_empty());
                c.input.update(cx, |s, cx| {
                    s.set_selected_range(0..3, cx);
                    s.replace_and_mark_text_in_range(None, "@he", Some(3..3), window, cx);
                });
                c.refresh_suggestions(window, cx);
                assert!(c.composing);
                assert!(c.suggestion_matches(cx).is_empty());
            });
            window.press("tab", cx);
            assert!(!composer.read(cx).text(cx).contains("@here"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn editing_restores_the_draft_and_keeps_its_uploads(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        let events = Rc::new(RefCell::new(Vec::new()));
        let _subscription = cx.update(|cx| {
            let events = events.clone();
            cx.subscribe(&composer, move |_, event, _| {
                if let ComposerEvent::Submit { text, files } = event {
                    events.borrow_mut().push((text.clone(), files.len()));
                }
            })
        });
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| {
                for id in 0..3 {
                    c.files.push(PendingFile {
                        local_id: id,
                        name: format!("draft-{id}.txt").into(),
                        size: 10,
                        source: FileSource::Path("draft.txt".into()),
                        preview: None,
                        _preview: None,
                        status: match id {
                            0 => AttachmentStatus::Uploading,
                            1 => AttachmentStatus::Failed,
                            _ => AttachmentStatus::Complete,
                        },
                        uploaded: (id == 2).then(FileInfo::default),
                        error: (id == 1).then(|| "Upload failed".into()),
                        _upload: None,
                    });
                }
                c.set_editing(Some("Original"), window, cx);
                c.set_editing(Some("Another message"), window, cx);
                assert_eq!(c.draft(cx), "Unsent draft");
            });
            window.press("escape", cx);
            assert_eq!(composer.read(cx).text(cx), "Unsent draft");
            composer.update(cx, |c, cx| c.set_editing(Some("Original"), window, cx));
            window.click("cancel-edit", cx);
            assert_eq!(composer.read(cx).text(cx), "Unsent draft");
            composer.update(cx, |c, cx| c.set_editing(Some("Saved edit"), window, cx));
            window.click("send", cx);
        })
        .unwrap();
        cx.run_until_parked();
        composer.read_with(cx, |c, cx| {
            assert!(!c.editing);
            assert_eq!(c.text(cx), "Unsent draft");
            assert_eq!(c.files.len(), 3);
            assert!(c.files[0].uploaded.is_none());
            assert!(c.files[1].error.is_some());
            assert!(c.files[2].uploaded.is_some());
        });
        assert_eq!(&*events.borrow(), &[("Saved edit".into(), 0)]);
    }

    #[gpui_kit::test]
    fn command_refresh_keeps_selection_and_internal_slashes_complete(cx: &mut TestAppContext) {
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(wait);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let server = SlowServer::start(Network::new(Duration::ZERO), move |request| {
            if request.method() == "GET"
                && request.path() == "/api/v4/teams/team/commands/autocomplete"
            {
                let refresh = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
                if refresh {
                    wait.lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap();
                    Reply::json(json!([
                        {"trigger":"ops/abort"}, {"trigger":"ops/deploy"}, {"trigger":"ops/status"}
                    ]))
                } else {
                    Reply::json(json!([{"trigger":"ops/deploy"}, {"trigger":"ops/status"}]))
                }
            } else {
                route(request)
            }
        });
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        let session = composer.read_with(cx, |c, _| c.session.clone().unwrap());
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.autocomplete_commands().len() == 2)
        });
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("/ops/d", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.press("tab", cx);
            assert_eq!(composer.read(cx).text(cx), "/ops/deploy ");
            session.update(cx, |s, cx| {
                s.select_team("other".into(), cx);
                s.select_team("team".into(), cx);
            });
            composer.update(cx, |c, cx| c.set_text("/ops/", window, cx));
        })
        .unwrap();
        wait_until(cx, |_| {
            server
                .requests()
                .iter()
                .filter(|r| {
                    r.method() == "GET" && r.path() == "/api/v4/teams/team/commands/autocomplete"
                })
                .count()
                == 2
        });
        cx.update_window(handle, |_, window, cx| {
            window.press("down", cx);
            assert_eq!(window.find("command-ops/status").selected(), Some(true));
        })
        .unwrap();
        release.send(()).unwrap();
        wait_until(cx, |cx| {
            session.read_with(cx, |s, _| s.autocomplete_commands().len() == 3)
        });
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            assert_eq!(window.find("command-ops/status").selected(), Some(true));
            assert_eq!(window.find("command-ops/deploy").selected(), Some(false));
            window.press("tab", cx);
            assert_eq!(composer.read(cx).text(cx), "/ops/status ");
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn server_slash_commands_complete_and_extensions_take_precedence(cx: &mut TestAppContext) {
        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            if request.method() == "GET"
                && request.path() == "/api/v4/teams/team/commands/autocomplete"
            {
                Reply::json(json!([
                    {"trigger":"poll", "auto_complete_hint":"[question]", "auto_complete_desc":"Create a poll"},
                    {"trigger":"shrug", "auto_complete_desc":"Server collision"}
                ]))
            } else {
                route(request)
            }
        });
        let (handle, composer) = open_connected_composer(Some(&server), cx);
        wait_until(cx, |cx| {
            composer.read_with(cx, |c, cx| {
                c.session
                    .as_ref()
                    .unwrap()
                    .read(cx)
                    .autocomplete_commands()
                    .len()
                    == 2
            })
        });
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("/po", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            let matches = composer.read(cx).suggestion_matches(cx);
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0].label, "/poll");
            assert_eq!(matches[0].detail, "[question] Create a poll");
            assert!(window.find("command-poll").selected().unwrap());
            window.press("tab", cx);
            assert_eq!(composer.read(cx).text(cx), "/poll ");
            composer.update(cx, |c, cx| c.set_text("/shr", window, cx));
            let matches = composer.read(cx).suggestion_matches(cx);
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0].completion, "/shrug");
            assert!(!matches[0].detail.contains("Server collision"));
        })
        .unwrap();
    }

    #[gpui_kit::test]
    fn slash_suggestions_support_selection_completion_and_dismissal(cx: &mut TestAppContext) {
        let (handle, composer) = open_composer(cx);
        let edits = Rc::new(RefCell::new(0));
        let _subscription = cx.update(|cx| {
            let edits = edits.clone();
            cx.subscribe(&composer, move |_, event, _| {
                if matches!(event, ComposerEvent::EditLast) {
                    *edits.borrow_mut() += 1;
                }
            })
        });
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("/", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            let matches = composer.read(cx).suggestion_matches(cx);
            assert_eq!(matches.len(), 6);
            assert!(
                composer.focus_handle(cx).is_focused(window),
                "composer must be focused"
            );
            window.press("up", cx);
            assert_eq!(composer.read(cx).selected_suggestion, 5);
            window.press("down", cx);
            assert_eq!(composer.read(cx).selected_suggestion, 0);
            window.press("down", cx);
            assert_eq!(window.find(matches[1].id.clone()).selected(), Some(true));
            window.press("tab", cx);
            assert_eq!(
                composer.read(cx).text(cx),
                format!("{} ", matches[1].completion)
            );
            window.input("argument", cx);
            assert_eq!(
                composer.read(cx).text(cx),
                format!("{} argument", matches[1].completion)
            );
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("/", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| {
            window.press("escape", cx);
            assert!(composer.read(cx).suggestion_matches(cx).is_empty());
            assert_eq!(composer.read(cx).text(cx), "/");
            window.press("right", cx);
            window.input("s", cx);
        })
        .unwrap();
        cx.run_until_parked();
        composer.read_with(cx, |c, cx| {
            assert!(
                !c.suggestion_matches(cx).is_empty(),
                "text={:?}, dismissed={}",
                c.text(cx),
                c.dismissed.is_some()
            );
            assert_eq!(c.selected_suggestion, 0);
        });
        assert_eq!(*edits.borrow(), 0);
        cx.update_window(handle, |_, window, cx| {
            composer.update(cx, |c, cx| c.set_text("", window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(handle, |_, window, cx| window.press("up", cx))
            .unwrap();
        cx.run_until_parked();
        assert_eq!(*edits.borrow(), 1);
    }
}
