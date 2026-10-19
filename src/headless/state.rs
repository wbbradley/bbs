//! Per-agent state in `~/.local/state/bbs/headless/<agent>/`: the saved token and
//! harness session (`state.json`, 0600) and the event log (`log.jsonl`).

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::state_dir;

#[derive(Default, Deserialize, Serialize)]
pub struct State {
    /// The server the token belongs to; `None` for local mode.
    pub server: Option<String>,
    pub token: Option<String>,
    /// The harness that owns `session_id`.
    pub harness: Option<String>,
    pub session_id: Option<String>,
}

pub struct AgentDir {
    pub dir: PathBuf,
}

impl AgentDir {
    /// The directory for `agent`. Names are case-insensitive, so the directory
    /// name is lowercase.
    pub fn new(agent: &str) -> Result<AgentDir> {
        anyhow::ensure!(
            !agent.is_empty()
                && agent
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric())
                && agent
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
            "invalid agent name {agent:?}"
        );
        Ok(AgentDir {
            dir: state_dir()?
                .join("headless")
                .join(agent.to_ascii_lowercase()),
        })
    }

    pub fn create(&self) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Hold an exclusive lock on this agent's directory until the file is dropped.
    pub fn lock(&self) -> Result<std::fs::File> {
        let path = self.dir.join("run.lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                anyhow::bail!("another `bbs headless run` is driving this agent on this machine")
            }
            Err(std::fs::TryLockError::Error(err)) => {
                Err(err).with_context(|| format!("locking {}", path.display()))
            }
        }
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log.jsonl")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.dir.join("harness.stderr")
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn load(&self) -> Result<State> {
        let path = self.state_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .map_err(|_| anyhow::anyhow!("invalid state in {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, state: &State) -> Result<()> {
        write_private(&self.state_path(), &serde_json::to_vec_pretty(state)?)
    }
}

/// Atomically replace `path` with a 0600 file.
pub(super) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().expect("state path has a parent");
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)
        .with_context(|| format!("writing {}", path.display()))?;
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_map_to_safe_directories() {
        assert!(
            AgentDir::new("Margaux")
                .unwrap()
                .dir
                .ends_with("headless/margaux")
        );
        for bad in ["", "..", ".hidden", "a/b", "a b", "-x"] {
            assert!(AgentDir::new(bad).is_err(), "{bad:?}");
        }
    }
}
