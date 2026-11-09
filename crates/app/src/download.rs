//! Saving attachments to the Downloads folder.

use std::path::{Path, PathBuf};

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::{App, AppContext as _, Window};
use mattermost::{Client, FileInfo};

/// Download `file` into Downloads, then hand its path to `done`. The name
/// gets a numeric suffix rather than overwriting a file. A failure is
/// reported as a notification.
pub fn save(
    client: Client,
    file: FileInfo,
    window: &mut Window,
    cx: &mut App,
    done: impl FnOnce(&Path, &mut Window, &mut App) + 'static,
) {
    window
        .spawn(cx, async move |cx| {
            let result = async {
                let bytes = client.download_file(&file.id).await?;
                let name = file.name.clone();
                cx.background_spawn(async move {
                    let dir = dirs::download_dir()
                        .or_else(dirs::home_dir)
                        .ok_or_else(|| anyhow::anyhow!("no Downloads folder"))?;
                    let path = unique_path(&dir, &name);
                    std::fs::write(&path, bytes)?;
                    anyhow::Ok(path)
                })
                .await
            }
            .await;
            let _ = cx.update(|window, cx| match result {
                Ok(path) => done(&path, window, cx),
                Err(err) => window.push_notification(
                    Notification::error(format!("Couldn’t download {}. {err:#}", file.name)),
                    cx,
                ),
            });
        })
        .detach();
}

/// `dir/name`, or `dir/name (2)` and so on if that file exists.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let name = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let candidate = dir.join(&name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}
