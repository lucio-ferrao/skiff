//! What skiff keeps of its own: the space and the title of each session, and a file it touches on
//! every change so `skiff watch` wakes up.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct Spaces {
    /// Session name → space name.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,
    /// Session name → the name shown for it. The zmx name only identifies the session: zmx cannot
    /// rename one, and the agent's hooks find theirs by it.
    #[serde(default)]
    pub titles: BTreeMap<String, String>,
}

pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("skiff")
}

fn spaces_path() -> PathBuf {
    config_dir().join("spaces.toml")
}

impl Spaces {
    pub fn load() -> Spaces {
        std::fs::read_to_string(spaces_path())
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Writes through a temporary file, so a reader never sees half of it.
    pub fn save(&self) -> io::Result<()> {
        let path = spaces_path();
        std::fs::create_dir_all(config_dir())?;
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let temporary = path.with_extension("toml.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(temporary, path)
    }
}

/// The directory whose changes wake `skiff watch`.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("skiff")
}

/// Tells watchers that a session's labels changed, which zmx does not announce.
pub fn mark_changed() {
    let dir = runtime_dir();
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join("changed"), b"");
}
