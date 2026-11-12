//! State that survives restarts: the signed-in session and preferences.
//!
//! Stored as JSON in the platform config directory. The session token is
//! kept apart from it, in the system keychain (see [`crate::keychain`]).
//! Where there is no keychain, or it refuses, the token stays in the file,
//! which is why the file is written with owner-only permissions on Unix. A
//! token found in the file, as older versions saved it, moves to the keychain
//! with the next save.
//!
//! The file is read once; afterwards [`load`] answers from memory, so views
//! may read preferences while rendering. [`update`] changes the copy in
//! memory and hands the write to a background thread, so the UI thread never
//! waits on the disk.

use std::{
    fs,
    io::Write as _,
    path::PathBuf,
    sync::{LazyLock, Mutex, OnceLock, mpsc},
};

use mattermost::User;
use serde::{Deserialize, Serialize};

use crate::keychain;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub server: String,
    /// In memory always; in the file only when the keychain can't hold it.
    #[serde(skip_serializing_if = "String::is_empty")]
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Updates {
    /// Check for new releases on a schedule and install them in the background.
    #[default]
    Automatic,
    /// Only when asked, with Check for Updates.
    Manual,
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
    pub updates: Updates,
}

#[cfg(not(test))]
fn path() -> Option<PathBuf> {
    Some(dirs::config_dir()?.join("mtc").join("state.json"))
}

/// Tests keep state in memory, away from the real file.
#[cfg(test)]
fn path() -> Option<PathBuf> {
    None
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| Mutex::new(read()));

/// The server and token [`read`] found in the keychain, so the writer knows
/// which item to delete if the first save after launch signs out.
static KEYCHAIN_AT_LAUNCH: OnceLock<(String, String)> = OnceLock::new();

enum Write {
    Save(Box<State>),
    /// Reply once every earlier save is on disk.
    Flush(mpsc::Sender<()>),
}

/// The background writer. Saves that queue up while one is being written
/// collapse into the newest, and a save identical to the file is skipped.
static WRITER: LazyLock<mpsc::Sender<Write>> = LazyLock::new(|| {
    LazyLock::force(&STATE);
    let mut saver = Saver::new(KEYCHAIN_AT_LAUNCH.get().cloned());
    let (tx, rx) = mpsc::channel::<Write>();
    std::thread::Builder::new()
        .name("mtc-persist".into())
        .spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut latest = None;
                let mut flushes = Vec::new();
                for write in std::iter::once(first).chain(rx.try_iter()) {
                    match write {
                        Write::Save(state) => latest = Some(*state),
                        Write::Flush(done) => flushes.push(done),
                    }
                }
                if let Some(bytes) = latest.and_then(|state| saver.save(&state)) {
                    write_file(&bytes);
                }
                for done in flushes {
                    let _ = done.send(());
                }
            }
        })
        .expect("failed to start the state writer");
    tx
});

/// Turns states into file contents, keeping the keychain in step: the
/// signed-in server's token is saved there and left out of the file, and a
/// token no longer in use is deleted from it.
struct Saver {
    /// The last bytes handed out, to skip saves that change nothing.
    written: Option<Vec<u8>>,
    /// The server and token the keychain holds.
    in_keychain: Option<(String, String)>,
    /// A server and token the keychain refused, so it isn't asked again,
    /// and perhaps the user prompted again, on every save.
    refused: Option<(String, String)>,
}

impl Saver {
    fn new(in_keychain: Option<(String, String)>) -> Self {
        Self {
            written: None,
            in_keychain,
            refused: None,
        }
    }

    /// The file contents for `state`, or `None` if the file has them already.
    fn save(&mut self, state: &State) -> Option<Vec<u8>> {
        let session = state
            .session
            .as_ref()
            .map(|s| (s.server.clone(), s.token.clone()));
        match &session {
            Some(current) if self.in_keychain.as_ref() != Some(current) => {
                if self.refused.as_ref() != Some(current) {
                    match keychain::set(&current.0, &current.1) {
                        Ok(()) => {
                            if let Some((old, _)) = self.in_keychain.take()
                                && old != current.0
                            {
                                self.forget(&old);
                            }
                            self.in_keychain = Some(current.clone());
                        }
                        Err(err) => {
                            log::warn!("saving the token to the keychain: {err:#}");
                            self.refused = Some(current.clone());
                        }
                    }
                }
            }
            Some(_) => {}
            None => {
                if let Some((old, _)) = self.in_keychain.take() {
                    self.forget(&old);
                }
            }
        }

        let mut on_disk = state.clone();
        if let Some(saved) = on_disk.session.as_mut()
            && self.in_keychain.as_ref() == session.as_ref()
        {
            saved.token.clear();
        }
        match serde_json::to_vec_pretty(&on_disk) {
            Ok(bytes) if self.written.as_ref() != Some(&bytes) => {
                self.written = Some(bytes.clone());
                Some(bytes)
            }
            Ok(_) => None,
            Err(err) => {
                log::warn!("saving state: {err}");
                None
            }
        }
    }

    fn forget(&self, server: &str) {
        if let Err(err) = keychain::delete(server) {
            log::warn!("deleting the token from the keychain: {err:#}");
        }
    }
}

fn read() -> State {
    let Some(path) = path() else {
        return State::default();
    };
    let state = fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let (state, in_keychain) = restore_token(state);
    if let Some(in_keychain) = in_keychain {
        let _ = KEYCHAIN_AT_LAUNCH.set(in_keychain);
    }
    state
}

/// Fill in the saved session's token from the keychain, unless the file
/// still holds it. Also returns what the keychain held. A session whose token
/// can't be found is dropped, so the sign-in screen shows instead.
fn restore_token(mut state: State) -> (State, Option<(String, String)>) {
    let Some(session) = state.session.as_mut() else {
        return (state, None);
    };
    if !session.token.is_empty() {
        return (state, None);
    }
    match keychain::get(&session.server) {
        Ok(Some(token)) => {
            session.token = token;
            let in_keychain = (session.server.clone(), session.token.clone());
            (state, Some(in_keychain))
        }
        Ok(None) => {
            log::warn!("no token in the keychain for {}", session.server);
            state.session = None;
            (state, None)
        }
        Err(err) => {
            log::warn!("reading the token from the keychain: {err:#}");
            state.session = None;
            (state, None)
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_in(server: &str, token: &str) -> State {
        State {
            session: Some(SavedSession {
                server: server.into(),
                token: token.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn file(bytes: &[u8]) -> State {
        serde_json::from_slice(bytes).unwrap()
    }

    #[test]
    fn token_goes_to_the_keychain_not_the_file() {
        let server = "https://saved.example.com";
        let mut saver = Saver::new(None);
        let bytes = saver.save(&signed_in(server, "secret")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
        assert_eq!(keychain::get(server).unwrap().as_deref(), Some("secret"));

        let (restored, in_keychain) = restore_token(file(&bytes));
        assert_eq!(restored.session.unwrap().token, "secret");
        assert_eq!(in_keychain, Some((server.into(), "secret".into())));
    }

    #[test]
    fn token_from_an_older_file_moves_to_the_keychain() {
        let server = "https://older.example.com";
        let old = serde_json::to_vec(&signed_in(server, "legacy")).unwrap();
        let (state, in_keychain) = restore_token(file(&old));
        assert_eq!(state.session.as_ref().unwrap().token, "legacy");
        assert_eq!(in_keychain, None);

        let mut saver = Saver::new(in_keychain);
        let bytes = saver.save(&state).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("legacy"));
        assert_eq!(keychain::get(server).unwrap().as_deref(), Some("legacy"));
    }

    #[test]
    fn signing_out_deletes_the_token() {
        let server = "https://signout.example.com";
        keychain::set(server, "secret").unwrap();
        let mut saver = Saver::new(Some((server.into(), "secret".into())));
        saver.save(&State::default()).unwrap();
        assert_eq!(keychain::get(server).unwrap(), None);
    }

    #[test]
    fn switching_servers_deletes_the_old_token() {
        let (a, b) = (
            "https://a.switch.example.com",
            "https://b.switch.example.com",
        );
        let mut saver = Saver::new(None);
        saver.save(&signed_in(a, "one"));
        saver.save(&signed_in(b, "two"));
        assert_eq!(keychain::get(a).unwrap(), None);
        assert_eq!(keychain::get(b).unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn without_a_keychain_the_token_stays_in_the_file() {
        let server = "https://unavailable.example.com";
        let mut saver = Saver::new(None);
        let bytes = saver.save(&signed_in(server, "secret")).unwrap();
        let (restored, _) = restore_token(file(&bytes));
        assert_eq!(restored.session.unwrap().token, "secret");
    }

    #[test]
    fn a_session_without_its_token_is_dropped() {
        let saved = signed_in("https://missing.example.com", "");
        let (state, in_keychain) = restore_token(saved);
        assert!(state.session.is_none());
        assert_eq!(in_keychain, None);
    }

    #[test]
    fn unchanged_saves_write_nothing() {
        let mut saver = Saver::new(None);
        let state = signed_in("https://unchanged.example.com", "secret");
        assert!(saver.save(&state).is_some());
        assert!(saver.save(&state).is_none());
    }
}
