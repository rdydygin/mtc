//! State that survives restarts: the signed-in session and preferences.
//!
//! Stored as JSON in the platform config directory. The file holds a session
//! token, so it is written with owner-only permissions on Unix.
//!
//! The file is read once; afterwards [`load`] answers from memory, so views
//! may read preferences while rendering. [`update`] changes the copy in
//! memory and hands the write to a background thread, so the UI thread never
//! waits on the disk.

use std::{
    fs,
    io::Write as _,
    path::PathBuf,
    sync::{LazyLock, Mutex, mpsc},
};

use mattermost::User;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub server: String,
    pub token: String,
    pub team_id: Option<String>,
    /// The signed-in user as last seen, so a restored session can open the
    /// window before the server confirms the token.
    pub user: Option<User>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub session: Option<SavedSession>,
    /// The last server signed in to, prefilled on the sign-in screen.
    pub last_server: String,
    pub last_login: String,
    pub appearance: Appearance,
    /// Interface zoom as the theme base font size, in pixels.
    pub font_size: Option<f32>,
    /// Sidebar width in pixels, as older versions saved it. Read only when
    /// `sidebar_width_rem` is missing.
    pub sidebar_width: Option<f32>,
    /// Sidebar width in rems, so it follows the interface zoom.
    pub sidebar_width_rem: Option<f32>,
    pub last_channel: Option<String>,
}

fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("mtc").join("state.json"))
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(read()));

enum Write {
    Save(Box<State>),
    /// Reply once every earlier save is on disk.
    Flush(mpsc::Sender<()>),
}

/// The background writer. Saves that queue up while one is being written
/// collapse into the newest, and a save identical to the file is skipped.
static WRITER: LazyLock<mpsc::Sender<Write>> = LazyLock::new(|| {
    let (tx, rx) = mpsc::channel::<Write>();
    std::thread::Builder::new()
        .name("mtc-persist".into())
        .spawn(move || {
            let mut written: Option<Vec<u8>> = None;
            while let Ok(first) = rx.recv() {
                let mut latest = None;
                let mut flushes = Vec::new();
                for write in std::iter::once(first).chain(rx.try_iter()) {
                    match write {
                        Write::Save(state) => latest = Some(*state),
                        Write::Flush(done) => flushes.push(done),
                    }
                }
                if let Some(state) = latest {
                    match serde_json::to_vec_pretty(&state) {
                        Ok(bytes) if written.as_ref() != Some(&bytes) => {
                            write_file(&bytes);
                            written = Some(bytes);
                        }
                        Ok(_) => {}
                        Err(err) => log::warn!("saving state: {err}"),
                    }
                }
                for done in flushes {
                    let _ = done.send(());
                }
            }
        })
        .expect("failed to start the state writer");
    tx
});

fn read() -> State {
    let Some(path) = path() else {
        return State::default();
    };
    fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// The current state, from memory.
pub fn load() -> State {
    STATE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Read one part of the state without copying the rest.
pub fn read_with<R>(read: impl FnOnce(&State) -> R) -> R {
    read(&STATE.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Change the state and save it in the background.
pub fn update(change: impl FnOnce(&mut State)) {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    change(&mut state);
    let _ = WRITER.send(Write::Save(Box::new(state.clone())));
}

/// Wait until every change so far is on disk, such as before quitting.
pub fn flush() {
    let (done, wait) = mpsc::channel();
    if WRITER.send(Write::Flush(done)).is_ok() {
        let _ = wait.recv();
    }
}

fn write_file(bytes: &[u8]) {
    let Some(path) = path() else {
        return;
    };
    let result = (|| -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let mut options = fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(tmp, &path)
    })();
    if let Err(err) = result {
        log::warn!("saving state: {err}");
    }
}
