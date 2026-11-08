//! The message field.
//!
//! Enter sends, Shift-Enter adds a line, Up in an empty field edits your
//! last message, Escape cancels an edit, and Tab completes a slash command.
//! The composer only reports intent; its owner decides what sending means.
//!
//! Files come in through the attach button, drag and drop, or pasting an
//! image. Each uploads at once, so sending is instant; a message can't be
//! sent while an upload is still running.

use gpui_kit::assets::IconName;
use std::{path::PathBuf, sync::Arc};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
    attachment::{
        Attachment, AttachmentContent, AttachmentDescription, AttachmentMedia, AttachmentStatus,
        AttachmentTitle,
    },
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{InputEvent, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::{
    AppContext as _, ClipboardEntry, ClipboardItem, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Image, ImageFormat, ImageSource, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, PathPromptOptions, Render, SharedString, StatefulInteractiveElement as _,
    Styled as _, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use mattermost::{ChannelId, Client, FileInfo};

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
    is_image: bool,
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
    /// Escape was pressed while editing.
    CancelEdit,
    /// The text changed; others may be told the user is typing.
    Typing,
}

pub struct Composer {
    input: Entity<TextareaState>,
    editing: bool,
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
            input.update(cx, |state, cx| state.set_value(draft, window, cx));
        }
        let subscription =
            cx.subscribe_in(
                &input,
                window,
                |this, input, event, window, cx| match event {
                    InputEvent::PressEnter { shift: false, .. } => this.submit(window, cx),
                    InputEvent::Change => {
                        if !input.read(cx).value().is_empty() {
                            cx.emit(ComposerEvent::Typing);
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            );
        Self {
            input,
            editing: false,
            target,
            files: Vec::new(),
            next_file_id: 0,
            _subscriptions: vec![subscription],
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
        !self.is_uploading() && (has_text || (has_files && !self.editing))
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
        self.set_text("", window, cx);
        self.editing = false;
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
            let is_image = image_extension(&name);
            self.add_file(name, metadata.len(), FileSource::Path(path), is_image, cx);
        }
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
                    self.add_file(
                        name,
                        size,
                        FileSource::Pasted(Arc::new(image.clone())),
                        true,
                        cx,
                    );
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
        is_image: bool,
        cx: &mut Context<Self>,
    ) {
        let local_id = self.next_file_id;
        self.next_file_id += 1;
        self.files.push(PendingFile {
            local_id,
            name: name.into(),
            size,
            source,
            is_image,
            status: AttachmentStatus::Uploading,
            uploaded: None,
            error: None,
            _upload: None,
        });
        self.upload(local_id, cx);
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
                    .background_spawn(async move { std::fs::read(path) })
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
                            file.error = Some(format!("{err:#}").into());
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
        if self.files.is_empty() {
            return None;
        }
        let entity = cx.entity().downgrade();
        Some(
            h_flex()
                .flex_wrap()
                .gap_2()
                .px_2()
                .pt_2()
                .children(self.files.iter().map(|file| render_file(file, &entity))),
        )
    }

    pub fn text(&self, cx: &gpui_kit::App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn set_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |state, cx| {
            state.set_value(text.to_string(), window, cx)
        });
        cx.notify();
    }

    /// Enter edit mode with `text`, or leave it.
    pub fn set_editing(&mut self, text: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = text.is_some();
        self.set_text(text.unwrap_or_default(), window, cx);
        self.focus_handle(cx).focus(window, cx);
    }

    pub fn is_editing(&self) -> bool {
        self.editing
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let plain = !keystroke.modifiers.modified();
        match keystroke.key.as_str() {
            "up" if plain && self.input.read(cx).value().is_empty() => {
                cx.emit(ComposerEvent::EditLast);
                cx.stop_propagation();
            }
            "escape" if self.editing => {
                self.editing = false;
                self.set_text("", window, cx);
                cx.emit(ComposerEvent::CancelEdit);
                cx.stop_propagation();
            }
            "tab" if plain => {
                if let Some(completion) = self.command_matches(cx).first().map(|c| c.0.clone()) {
                    self.set_text(&format!("/{completion} "), window, cx);
                    cx.stop_propagation();
                }
            }
            _ => {}
        }
    }

    /// Slash commands matching a partially typed `/name`.
    fn command_matches(
        &self,
        cx: &gpui_kit::App,
    ) -> Vec<(SharedString, SharedString, SharedString)> {
        let text = self.input.read(cx).value();
        let Some(prefix) = text.strip_prefix('/') else {
            return Vec::new();
        };
        if prefix.contains(char::is_whitespace) {
            return Vec::new();
        }
        extensions::slash_commands(cx)
            .iter()
            .filter(|c| c.name().starts_with(prefix))
            .take(6)
            .map(|c| {
                (
                    c.name().clone(),
                    c.hint_text().clone(),
                    c.description().clone(),
                )
            })
            .collect()
    }

    fn render_command_matches(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let _ = window;
        let matches = self.command_matches(cx);
        if matches.is_empty() {
            return None;
        }
        let theme = cx.theme();
        Some(
            v_flex()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .children(matches.into_iter().enumerate().map(
                    |(ix, (name, hint, description))| {
                        let completion = name.clone();
                        h_flex()
                            .id(SharedString::from(format!("command-{name}")))
                            .gap_2()
                            .px_3()
                            .py_1()
                            .text_sm()
                            .when(ix == 0, |this| this.bg(theme.accent))
                            .hover(|style| style.bg(theme.accent))
                            .child(div().font_semibold().child(format!("/{name}")))
                            .when(!hint.is_empty(), |this| {
                                this.child(div().text_color(theme.muted_foreground).child(hint))
                            })
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme.muted_foreground)
                                    .child(description),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.set_text(&format!("/{completion} "), window, cx);
                                this.focus_handle(cx).focus(window, cx);
                            }))
                    },
                )),
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
        let command_matches = self
            .render_command_matches(window, cx)
            .map(IntoElement::into_any_element);
        let files = self.render_files(cx).map(IntoElement::into_any_element);
        let can_submit = self.can_submit(cx);
        let uploading = self.is_uploading();
        let editing = self.editing;
        let entity = cx.entity().downgrade();
        let theme = cx.theme();
        // One filled field, like the composers people already know: the
        // attach and send actions sit inside it, beside the text.
        div().px_4().pb_4().child(
            v_flex()
                .capture_key_down(cx.listener(Self::on_key_down))
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
                            .child("Editing message")
                            .child("·")
                            .child("Escape to cancel"),
                    )
                })
                .children(command_matches)
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

fn render_file(file: &PendingFile, entity: &WeakEntity<Composer>) -> impl IntoElement + use<> {
    let local_id = file.local_id;
    let media = match (&file.source, file.is_image) {
        (FileSource::Pasted(image), _) => {
            AttachmentMedia::new().src(ImageSource::Image(image.clone()))
        }
        (FileSource::Path(path), true) => AttachmentMedia::new().src(path.clone()),
        (FileSource::Path(_), false) => AttachmentMedia::new().child(Icon::new(IconName::File)),
    };
    let description = match &file.error {
        Some(error) => error.clone(),
        None => format_size(file.size).into(),
    };
    let remove = entity.clone();
    let retry = entity.clone();
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
        })
        .on_retry(move |_, _, cx| {
            let _ = retry.update(cx, |this, cx| this.upload(local_id, cx));
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
