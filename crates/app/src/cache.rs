//! What is worth keeping between runs but can always be fetched again: the
//! server's images, the people last seen, and the custom emoji. With them,
//! a restart shows avatars, custom emoji, attachment previews, and names as
//! soon as the posts that need them arrive, rather than one or two round
//! trips later.
//!
//! Kept in the platform cache directory, readable only by the user, and
//! deleted when the session ends, by sign-out or by the server. Reads and
//! writes block, so callers run them off the UI thread; [`remember_users`]
//! and [`remember_emoji`] do that themselves.

use std::{
    collections::HashMap,
    fs,
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, SystemTime},
};

use gpui_kit::{App, AppContext as _, Context, Entity, Subscription, Task};
use mattermost::User;
use serde::de::DeserializeOwned;
use session::{Directory, EmojiStore};
use sha2::{Digest as _, Sha256};

/// The most image data kept; the least recently used goes first. Checked
/// on every save, so a channel full of large attachments can't fill the
/// disk during a long session.
const IMAGE_LIMIT: u64 = 256 * 1024 * 1024;
/// How long the people and emoji wait after a change before they are
/// saved, so the burst of lookups at startup costs one write.
const SAVE_DELAY: Duration = Duration::from_secs(5);

/// The sign-in that writes belong to. [`clear`] moves it on, so a write
/// begun for an account signed out of, such as a download still in
/// flight, is dropped rather than putting back what was deleted. Writes
/// hold it for reading while they check and write.
static GENERATION: RwLock<u64> = RwLock::new(0);

/// Caches of ended sessions being deleted, for [`flush`] to wait on.
static DELETIONS: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// The cache in use sits in this folder of [`root`]; an ended session's is
/// renamed to `ended-…` beside it until it is deleted.
const CURRENT: &str = "current";
const ENDED: &str = "ended-";

/// About how much image data is saved: measured when pruning, counted up
/// by each save in between.
static IMAGE_BYTES: AtomicU64 = AtomicU64::new(0);

/// The current sign-in, for [`save_image`].
pub fn generation() -> u64 {
    *GENERATION.read().unwrap_or_else(|e| e.into_inner())
}

#[cfg(not(test))]
fn root() -> Option<PathBuf> {
    Some(dirs::cache_dir()?.join("mtc"))
}

/// Tests never touch the real cache.
#[cfg(test)]
fn root() -> Option<PathBuf> {
    None
}

fn dir() -> Option<PathBuf> {
    Some(root()?.join(CURRENT))
}

fn key(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn images() -> Option<PathBuf> {
    Some(dir()?.join("images"))
}

/// The image saved for `url`, if any.
pub fn image(url: &str) -> Option<Vec<u8>> {
    read_marking_used(&images()?.join(key(url)))
}

/// Save `bytes` as the image for `url`, unless the cache was cleared
/// since `generation`.
pub fn save_image(generation: u64, url: &str, bytes: &[u8]) {
    if let Some(images) = images() {
        save_image_in(&images, IMAGE_LIMIT, generation, url, bytes);
    }
}

/// [`save_image`] into `images`, keeping them within `limit` bytes.
fn save_image_in(images: &Path, limit: u64, generation: u64, url: &str, bytes: &[u8]) {
    let len = bytes.len() as u64;
    // A few huge images would push out everything else.
    if len > limit / 32 {
        return;
    }
    match write_for(generation, &images.join(key(url)), bytes) {
        Ok(true) => {
            if IMAGE_BYTES.fetch_add(len, Ordering::Relaxed) + len > limit {
                prune_images(images, limit);
            }
        }
        Ok(false) => {}
        Err(err) => log::warn!("caching an image: {err}"),
    }
}

/// Bring `images` within `limit`, well within, so the next prune is many
/// saves away.
fn prune_images(images: &Path, limit: u64) {
    static PRUNING: Mutex<()> = Mutex::new(());
    // Another thread is already at it.
    let Ok(_pruning) = PRUNING.try_lock() else {
        return;
    };
    let left = prune(images, limit, limit / 4 * 3);
    IMAGE_BYTES.store(left, Ordering::Relaxed);
}

/// Measure the saved images and keep them within [`IMAGE_LIMIT`], and
/// finish deleting caches of sessions that ended in an earlier run.
pub fn tidy() {
    if let Some(root) = root() {
        sweep_ended(&root, &format!("{ENDED}{}-", std::process::id()));
        prune_images(&root.join(CURRENT).join("images"), IMAGE_LIMIT);
    }
}

/// Forget everything saved, as when a session ends. Nothing can read the
/// cache once this returns, and writes begun before it are dropped, even
/// ones that finish after it. The files are deleted on a thread that
/// [`flush`] waits for.
pub fn clear() {
    // Held while the cache is moved, so no write is halfway into it.
    let mut generation = GENERATION.write().unwrap_or_else(|e| e.into_inner());
    *generation += 1;
    let Some(root) = root() else {
        return;
    };
    let name = format!("{ENDED}{}-{}", std::process::id(), *generation);
    match set_aside(&root, &name) {
        Ok(Some(ended)) => {
            let deletion = std::thread::spawn(move || {
                if let Err(err) = fs::remove_dir_all(&ended) {
                    log::warn!("deleting the cache of an ended session: {err}");
                }
            });
            DELETIONS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(deletion);
        }
        Ok(None) => {}
        // Better a slow sign-out than another account reading this one's.
        Err(err) => {
            log::warn!("setting the cache aside: {err}");
            if let Err(err) = fs::remove_dir_all(root.join(CURRENT)) {
                log::warn!("deleting the cache: {err}");
            }
        }
    }
}

/// Wait until the caches of ended sessions are deleted, such as before
/// quitting.
pub fn flush() {
    let deletions = std::mem::take(&mut *DELETIONS.lock().unwrap_or_else(|e| e.into_inner()));
    for deletion in deletions {
        let _ = deletion.join();
    }
}

/// Fill `directory` with the people saved last time on `server`, and save
/// them again whenever they change. Dropping the subscription stops saving.
pub fn remember_users(
    directory: &Entity<Directory>,
    server: &str,
    cx: &mut App,
) -> Option<Subscription> {
    let path = dir()?.join(format!("users-{}.json", key(server)));
    restore(
        directory,
        &path,
        |directory, users: Vec<User>, cx| directory.remember(users, cx),
        cx,
    );
    let mut saved = directory.read(cx).revision();
    let mut later = SaveLater::new(path);
    Some(cx.observe(directory, move |directory, cx| {
        // Presence changes notify too; only a change to the people counts.
        let directory = directory.read(cx);
        if directory.revision() != saved {
            saved = directory.revision();
            let users: Vec<Arc<User>> = directory.users().cloned().collect();
            later.save(move || serde_json::to_vec(&users), cx);
        }
    }))
}

/// Fill `store` with the custom emoji saved last time on `server`, and save
/// them again whenever they change. Dropping the subscription stops saving.
pub fn remember_emoji(
    store: &Entity<EmojiStore>,
    server: &str,
    cx: &mut App,
) -> Option<Subscription> {
    let path = dir()?.join(format!("emoji-{}.json", key(server)));
    restore(
        store,
        &path,
        |store, emoji: HashMap<String, String>, cx| store.remember(emoji, cx),
        cx,
    );
    let mut later = SaveLater::new(path);
    // The store notifies only when its emoji change.
    Some(cx.observe(store, move |store, cx| {
        let custom = store.read(cx).custom().clone();
        later.save(
            move || serde_json::to_vec(&custom.iter().collect::<HashMap<_, _>>()),
            cx,
        );
    }))
}

/// Read the JSON saved at `path` in the background, then hand it to
/// `apply`. A missing or unreadable file reads as empty.
fn restore<E: 'static, T: DeserializeOwned + Default + Send + 'static>(
    entity: &Entity<E>,
    path: &Path,
    apply: impl FnOnce(&mut E, T, &mut Context<E>) + 'static,
    cx: &mut App,
) {
    let path = path.to_path_buf();
    let load = cx.background_spawn(async move {
        fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    });
    let entity = entity.downgrade();
    cx.spawn(async move |cx| {
        let value = load.await;
        let _ = entity.update(cx, |entity, cx| apply(entity, value, cx));
    })
    .detach();
}

/// Writes one file [`SAVE_DELAY`] after the last change asked for, so a
/// burst of changes costs one write.
struct SaveLater {
    path: PathBuf,
    generation: u64,
    pending: Option<Task<()>>,
}

impl SaveLater {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            generation: generation(),
            pending: None,
        }
    }

    /// Save what `contents` makes, unless another save replaces this one
    /// first. `contents` runs in the background.
    fn save(
        &mut self,
        contents: impl FnOnce() -> serde_json::Result<Vec<u8>> + Send + 'static,
        cx: &App,
    ) {
        let path = self.path.clone();
        let generation = self.generation;
        let executor = cx.background_executor().clone();
        // Replacing the task cancels the save it was waiting to make.
        self.pending = Some(cx.background_spawn(async move {
            executor.timer(SAVE_DELAY).await;
            let result = contents()
                .map_err(std::io::Error::other)
                .and_then(|bytes| write_for(generation, &path, &bytes));
            if let Err(err) = result {
                log::warn!("saving {}: {err}", path.display());
            }
        }));
    }
}

/// Rename the cache in use in `root` to `name`, out of reach of every
/// later read; where it went, or `None` when there was none.
fn set_aside(root: &Path, name: &str) -> std::io::Result<Option<PathBuf>> {
    let ended = root.join(name);
    match fs::rename(root.join(CURRENT), &ended) {
        Ok(()) => Ok(Some(ended)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    }
}

/// Delete the caches set aside in `root` by an earlier run, as one that
/// quit or crashed before deleting them. Those named with `ours` belong to
/// this run, which deletes them itself.
fn sweep_ended(root: &Path, ours: &str) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(ENDED)
            && !name.starts_with(ours)
            && let Err(err) = fs::remove_dir_all(entry.path())
        {
            log::warn!("deleting the cache of an ended session: {err}");
        }
    }
}

/// A file's contents, refreshing its modification time so pruning sees
/// it as recently used. An empty file, as a crash may leave, is a miss.
fn read_marking_used(path: &Path) -> Option<Vec<u8>> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let _ = file.set_modified(SystemTime::now());
    (!bytes.is_empty()).then_some(bytes)
}

/// [`write`], unless the cache was cleared since `generation`. Returns
/// whether it wrote.
fn write_for(generation: u64, path: &Path, bytes: &[u8]) -> std::io::Result<bool> {
    let current = GENERATION.read().unwrap_or_else(|e| e.into_inner());
    if *current != generation {
        return Ok(false);
    }
    write(path, bytes).map(|()| true)
}

/// Write `path` whole or not at all, readable only by the user.
fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Unique per write, so two writes of one file can't mix.
    static NEXT: AtomicU64 = AtomicU64::new(0);
    if let Some(dir) = path.parent() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(dir)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", NEXT.fetch_add(1, Ordering::Relaxed)));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let result = options
        .open(&tmp)
        .and_then(|mut file| file.write_all(bytes))
        .and_then(|()| fs::rename(&tmp, path));
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// If the files in `dir` take more than `over` bytes, delete the least
/// recently used until the rest take at most `to`. Returns the bytes left.
fn prune(dir: &Path, over: u64, to: u64) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok().filter(|meta| meta.is_file())?;
            Some((meta.modified().ok()?, meta.len(), entry.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total <= over {
        return total;
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, len, path) in files {
        if total <= to {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total -= len;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mtc-cache-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn written_files_read_back_and_empty_ones_miss() {
        let dir = scratch("write");
        let path = dir.join("images").join(key("https://chat.example.com/a"));
        write(&path, b"png").unwrap();
        assert_eq!(read_marking_used(&path).as_deref(), Some(&b"png"[..]));
        write(&path, b"").unwrap();
        assert_eq!(read_marking_used(&path), None);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().unwrap()), 0o700);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn writes_begun_before_a_clear_are_dropped() {
        let dir = scratch("clear");
        let before = generation();
        clear();

        assert!(!write_for(before, &dir.join("old"), b"png").unwrap());
        assert!(write_for(generation(), &dir.join("new"), b"png").unwrap());

        assert!(!dir.join("old").exists());
        assert!(dir.join("new").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn an_ended_cache_is_out_of_reach_at_once_and_swept_later() {
        let root = scratch("ended");
        write(&root.join(CURRENT).join("users.json"), b"[]").unwrap();

        let ended = set_aside(&root, "ended-1-1").unwrap().unwrap();
        assert!(!root.join(CURRENT).exists(), "nothing reads it again");
        assert!(set_aside(&root, "ended-1-2").unwrap().is_none());

        // An earlier run's leftovers go; this run's are left to it.
        write(&root.join("ended-2-1").join("users.json"), b"[]").unwrap();
        sweep_ended(&root, "ended-2-");
        assert!(!ended.exists());
        assert!(root.join("ended-2-1").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pruning_drops_the_least_recently_used_first() {
        let dir = scratch("prune");
        let day = Duration::from_secs(24 * 60 * 60);
        let now = SystemTime::now();
        for (name, age) in [("old", 3), ("used", 2), ("new", 1)] {
            write(&dir.join(name), &[0; 10]).unwrap();
            let file = fs::File::options()
                .write(true)
                .open(dir.join(name))
                .unwrap();
            file.set_modified(now - day * age).unwrap();
        }
        // Reading "old" makes it the most recently used.
        read_marking_used(&dir.join("old")).unwrap();

        assert_eq!(prune(&dir, 25, 20), 20);

        assert!(dir.join("old").exists());
        assert!(!dir.join("used").exists());
        assert!(dir.join("new").exists());
        assert_eq!(prune(&dir, 25, 10), 20, "within the limit, nothing goes");
        assert!(dir.join("new").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saved_images_stay_within_the_limit() {
        let images = scratch("limit");
        // Another test's clear may drop a save before the folder exists.
        fs::create_dir_all(&images).unwrap();
        let limit = 32 * 100;
        let size = |dir: &Path| -> u64 {
            fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().metadata().unwrap().len())
                .sum()
        };
        for n in 0..200 {
            let url = format!("https://chat.example.com/api/v4/files/f{n}");
            save_image_in(&images, limit, generation(), &url, &[0; 100]);
            assert!(size(&images) <= limit, "after {n} saves");
        }
        save_image_in(&images, limit, generation(), "https://x/huge", &[0; 101]);
        assert!(
            !images.join(key("https://x/huge")).exists(),
            "too big to keep"
        );
        fs::remove_dir_all(images).unwrap();
    }
}
