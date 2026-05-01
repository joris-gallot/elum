//! Host book persistence: a versioned JSON file at the OS data directory.
//!
//! macOS path: `~/Library/Application Support/elum/hosts.json`.
//!
//! V0 schema:
//! ```jsonc
//! {
//!   "version": 1,
//!   "hosts": [
//!     {
//!       "id": "docker-test",
//!       "name": "Docker Test",
//!       "host": "127.0.0.1",
//!       "port": 2222,
//!       "user": "testuser",
//!       "key_path": "/abs/path/to/id_ed25519"
//!     }
//!   ]
//! }
//! ```
//!
//! Schema-version is on disk so we can migrate forward without breaking
//! older user files. Loading an unknown version is treated as an error so
//! we never silently drop user state.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

/// One configured SSH endpoint. Identified by `id` so tabs can survive
/// rename/edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
  pub id: String,
  pub name: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub key_path: PathBuf,
}

/// Container for the on-disk file. Schema-version on the wire so the next
/// migration doesn't have to guess.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostBookFile {
  version: u32,
  hosts: Vec<Host>,
}

const SCHEMA_VERSION: u32 = 1;

/// Live, in-memory host book. The path is kept for `save()`.
#[derive(Debug)]
pub struct HostBook {
  path: PathBuf,
  hosts: Vec<Host>,
}

#[allow(dead_code)] // Several methods are public surface for the future
                    // edit-host UI; binary crates flag pub-but-unreached as
                    // dead, so we silence the lint at the impl block.
impl HostBook {
  /// Standard location: `~/Library/Application Support/elum/hosts.json` on
  /// macOS, `$XDG_CONFIG_HOME/elum/hosts.json` on Linux. Falls back to a
  /// path under the binary's working directory if no data dir is found.
  pub fn default_path() -> PathBuf {
    dirs::data_dir().map_or_else(
      || PathBuf::from("./elum-hosts.json"),
      |p| p.join("elum").join("hosts.json"),
    )
  }

  /// Load from `path`. If the file is absent, returns an empty book.
  /// Errors only on present-but-corrupt files or unknown schema versions.
  pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
    let path = path.as_ref().to_path_buf();
    match fs::read_to_string(&path) {
      Ok(text) => {
        let file: HostBookFile =
          serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if file.version != SCHEMA_VERSION {
          return Err(anyhow!(
            "unknown host-book schema version {} at {}",
            file.version,
            path.display()
          ));
        }
        Ok(Self {
          path,
          hosts: file.hosts,
        })
      }
      Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self {
        path,
        hosts: Vec::new(),
      }),
      Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
  }

  /// Convenience wrapper around the OS default path.
  pub fn load_default() -> Result<Self> {
    Self::load_from(Self::default_path())
  }

  pub fn hosts(&self) -> &[Host] {
    &self.hosts
  }

  pub fn path(&self) -> &Path {
    &self.path
  }

  pub fn is_empty(&self) -> bool {
    self.hosts.is_empty()
  }

  pub fn add(&mut self, host: Host) {
    self.hosts.push(host);
  }

  /// Replace the host at `index`, panicking if out of bounds.
  pub fn replace(&mut self, index: usize, host: Host) {
    self.hosts[index] = host;
  }

  /// Remove the host at `index`. No-op if `index` is out of bounds,
  /// safer than panic for UI-driven calls.
  pub fn remove(&mut self, index: usize) {
    if index < self.hosts.len() {
      self.hosts.remove(index);
    }
  }

  /// Persist the current state to disk. Creates parent directories as
  /// needed; writes atomically through a sibling temp file + rename so a
  /// crash mid-write can't corrupt the user's host list.
  pub fn save(&self) -> Result<()> {
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent)
        .with_context(|| format!("creating directory {}", parent.display()))?;
    }

    let file = HostBookFile {
      version: SCHEMA_VERSION,
      hosts: self.hosts.clone(),
    };
    let text = serde_json::to_string_pretty(&file).context("serializing host book")?;

    // Atomic write: write to a sibling temp file then rename over the
    // target so a crash never leaves a half-written hosts.json.
    let tmp = self.path.with_extension("json.tmp");
    fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &self.path)
      .with_context(|| format!("renaming {} -> {}", tmp.display(), self.path.display()))?;
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  fn host(id: &str) -> Host {
    Host {
      id: id.to_string(),
      name: format!("Host {id}"),
      host: "127.0.0.1".into(),
      port: 22,
      user: "user".into(),
      key_path: PathBuf::from("/dev/null"),
    }
  }

  #[test]
  fn missing_file_loads_empty_book() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    let book = HostBook::load_from(&path).expect("load");
    assert!(book.is_empty());
    assert_eq!(book.path(), path);
  }

  #[test]
  fn add_save_load_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    let mut book = HostBook::load_from(&path).unwrap();
    book.add(host("a"));
    book.add(host("b"));
    book.save().unwrap();

    let reloaded = HostBook::load_from(&path).unwrap();
    assert_eq!(reloaded.hosts(), book.hosts());
  }

  #[test]
  fn save_creates_missing_parent_directories() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("nested").join("deeper").join("hosts.json");
    let mut book = HostBook::load_from(&path).unwrap();
    book.add(host("x"));
    book.save().expect("save creates parents");
    assert!(path.exists());
  }

  #[test]
  fn remove_at_index_drops_host() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    let mut book = HostBook::load_from(&path).unwrap();
    book.add(host("a"));
    book.add(host("b"));
    book.add(host("c"));
    book.remove(1);
    assert_eq!(book.hosts().len(), 2);
    assert_eq!(book.hosts()[0].id, "a");
    assert_eq!(book.hosts()[1].id, "c");
  }

  #[test]
  fn remove_out_of_bounds_is_noop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    let mut book = HostBook::load_from(&path).unwrap();
    book.add(host("a"));
    book.remove(99);
    assert_eq!(book.hosts().len(), 1);
  }

  #[test]
  fn unknown_schema_version_is_an_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    fs::write(&path, r#"{"version":99,"hosts":[]}"#).unwrap();
    let err = HostBook::load_from(&path).unwrap_err();
    assert!(err.to_string().contains("unknown host-book schema"));
  }

  #[test]
  fn malformed_json_is_an_error() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    fs::write(&path, "{ this is not json").unwrap();
    let err = HostBook::load_from(&path).unwrap_err();
    // Either the parse error chain mentions the path or just confirms
    // we propagated something non-trivial, either way, NOT a silent
    // empty load.
    assert!(err.to_string().contains("hosts.json") || err.chain().count() > 0);
  }
}
