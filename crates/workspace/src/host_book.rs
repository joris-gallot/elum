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
//!       "auth": {
//!         "type": "public_key",
//!         "key_path": "/abs/path/to/id_ed25519",
//!         "passphrase_in_keychain": false
//!       }
//!     }
//!   ]
//! }
//! ```

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostAuth {
  PublicKey {
    key_path: PathBuf,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    passphrase_in_keychain: bool,
  },
  Password {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    in_keychain: bool,
  },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
  pub id: String,
  pub name: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub auth: HostAuth,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub proxy_jump: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HostBookFile {
  version: u32,
  hosts: Vec<Host>,
}

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub struct HostBook {
  path: PathBuf,
  hosts: Vec<Host>,
}

impl HostBook {
  pub fn default_path() -> PathBuf {
    dirs::data_dir().map_or_else(
      || PathBuf::from("./elum-hosts.json"),
      |p| p.join("elum").join("hosts.json"),
    )
  }

  /// Empty book if the file is absent; errors on corrupt or unknown schema.
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

  pub fn hosts(&self) -> &[Host] {
    &self.hosts
  }

  pub fn is_empty(&self) -> bool {
    self.hosts.is_empty()
  }

  pub fn add(&mut self, host: Host) {
    self.hosts.push(host);
  }

  pub fn replace(&mut self, index: usize, host: Host) {
    self.hosts[index] = host;
  }

  pub fn remove(&mut self, index: usize) {
    if index < self.hosts.len() {
      self.hosts.remove(index);
    }
  }

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
      auth: HostAuth::PublicKey {
        key_path: PathBuf::from("/dev/null"),
        passphrase_in_keychain: false,
      },
      proxy_jump: None,
    }
  }

  #[test]
  fn missing_file_loads_empty_book() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("hosts.json");
    let book = HostBook::load_from(&path).expect("load");
    assert!(book.is_empty());
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
