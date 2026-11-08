//! State that survives restarts: the signed-in session and preferences.
//!
//! Stored as JSON in the platform config directory. The file holds a session
//! token, so it is written with owner-only permissions on Unix.

use std::{fs, io::Write as _, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedSession {
    pub server: String,
    pub token: String,
    pub team_id: Option<String>,
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

pub fn load() -> State {
    let Some(path) = path() else {
        return State::default();
    };
    fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn save(state: &State) {
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
        file.write_all(&serde_json::to_vec_pretty(state)?)?;
        file.sync_all()?;
        fs::rename(tmp, &path)
    })();
    if let Err(err) = result {
        log::warn!("saving state: {err}");
    }
}

/// Load, change, and save in one step.
pub fn update(change: impl FnOnce(&mut State)) {
    let mut state = load();
    change(&mut state);
    save(&state);
}
