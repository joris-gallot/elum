use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ssh2_config::{ParseRule, SshConfig};

use crate::host_book::{Host, HostAuth, HostBook};

#[derive(Debug, Clone, PartialEq)]
pub struct ImportCandidate {
  pub alias: String,
  pub host: String,
  pub port: u16,
  pub user: String,
  pub identity_file: Option<PathBuf>,
  pub proxy_jump: Option<String>,
  pub already_exists: bool,
  pub warnings: Vec<String>,
}

pub fn default_ssh_config_path() -> Option<PathBuf> {
  dirs::home_dir().map(|h| h.join(".ssh").join("config"))
}

pub fn parse_ssh_config(path: &Path, existing: &HostBook) -> Result<Vec<ImportCandidate>> {
  let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
  let mut reader = BufReader::new(file);
  let config = SshConfig::default()
    .parse(&mut reader, ParseRule::ALLOW_UNKNOWN_FIELDS)
    .with_context(|| format!("parsing {}", path.display()))?;

  let aliases = collect_named_aliases(&config);
  let candidates = aliases
    .into_iter()
    .map(|alias| build_candidate(&config, &alias, existing))
    .collect();
  Ok(candidates)
}

fn collect_named_aliases(config: &SshConfig) -> Vec<String> {
  let mut out = Vec::new();
  for host in config.get_hosts() {
    if host.pattern.len() != 1 {
      continue;
    }
    let clause = &host.pattern[0];
    if clause.negated {
      continue;
    }
    if is_wildcard(&clause.pattern) {
      continue;
    }
    if !out.contains(&clause.pattern) {
      out.push(clause.pattern.clone());
    }
  }
  out
}

fn is_wildcard(pattern: &str) -> bool {
  pattern.contains('*') || pattern.contains('?') || pattern.contains('!')
}

fn build_candidate(config: &SshConfig, alias: &str, existing: &HostBook) -> ImportCandidate {
  let params = config.query(alias);
  let host = params
    .host_name
    .clone()
    .unwrap_or_else(|| alias.to_string());
  let port = params.port.unwrap_or(22);
  let user = params
    .user
    .clone()
    .unwrap_or_else(|| std::env::var("USER").unwrap_or_default());
  let identity_file = params
    .identity_file
    .as_ref()
    .and_then(|files| files.first().cloned())
    .map(expand_tilde);

  let mut warnings = Vec::new();
  let proxy_jump = params.proxy_jump.as_ref().and_then(|chain| {
    if chain.len() > 1 {
      warnings.push("ProxyJump chain truncated to first host".into());
    }
    chain.first().cloned()
  });

  let already_exists = existing.hosts().iter().any(|h| {
    h.name.eq_ignore_ascii_case(alias) || (h.host == host && h.port == port && h.user == user)
  });

  ImportCandidate {
    alias: alias.to_string(),
    host,
    port,
    user,
    identity_file,
    proxy_jump,
    already_exists,
    warnings,
  }
}

fn expand_tilde(p: PathBuf) -> PathBuf {
  let s = p.to_string_lossy();
  if let Some(rest) = s.strip_prefix("~/") {
    if let Some(home) = dirs::home_dir() {
      return home.join(rest);
    }
  }
  p
}

pub fn candidate_to_host(id: String, c: &ImportCandidate) -> Host {
  let auth = HostAuth::PublicKey {
    key_path: c
      .identity_file
      .clone()
      .unwrap_or_else(default_identity_path),
    passphrase_in_keychain: false,
  };
  Host {
    id,
    name: c.alias.clone(),
    host: c.host.clone(),
    port: c.port,
    user: c.user.clone(),
    auth,
    proxy_jump: c.proxy_jump.clone(),
  }
}

fn default_identity_path() -> PathBuf {
  dirs::home_dir()
    .unwrap_or_default()
    .join(".ssh")
    .join("id_ed25519")
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Write;
  use tempfile::TempDir;

  fn write_config(dir: &TempDir, content: &str) -> PathBuf {
    let path = dir.path().join("config");
    let mut f = File::create(&path).unwrap();
    f.write_all(content.as_bytes()).unwrap();
    path
  }

  fn empty_book(dir: &TempDir) -> HostBook {
    HostBook::load_from(dir.path().join("hosts.json")).unwrap()
  }

  #[test]
  fn maps_basic_host_block() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
      &dir,
      "Host vps\n  HostName 1.2.3.4\n  User root\n  Port 2222\n  IdentityFile /tmp/key\n",
    );
    let cands = parse_ssh_config(&path, &empty_book(&dir)).unwrap();
    assert_eq!(cands.len(), 1);
    let c = &cands[0];
    assert_eq!(c.alias, "vps");
    assert_eq!(c.host, "1.2.3.4");
    assert_eq!(c.port, 2222);
    assert_eq!(c.user, "root");
    assert_eq!(c.identity_file, Some(PathBuf::from("/tmp/key")));
    assert!(!c.already_exists);
  }

  #[test]
  fn skips_wildcard_blocks() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
      &dir,
      "Host *\n  User shared\n\nHost real\n  HostName real.example.com\n",
    );
    let cands = parse_ssh_config(&path, &empty_book(&dir)).unwrap();
    let aliases: Vec<_> = cands.iter().map(|c| c.alias.as_str()).collect();
    assert_eq!(aliases, vec!["real"]);
  }

  #[test]
  fn alias_without_hostname_falls_back_to_alias() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, "Host bare\n  User foo\n");
    let cands = parse_ssh_config(&path, &empty_book(&dir)).unwrap();
    assert_eq!(cands[0].host, "bare");
    assert_eq!(cands[0].user, "foo");
  }

  #[test]
  fn tilde_in_identity_file_is_expanded() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
      &dir,
      "Host h\n  HostName example.com\n  IdentityFile ~/.ssh/foo\n",
    );
    let cands = parse_ssh_config(&path, &empty_book(&dir)).unwrap();
    let id = cands[0].identity_file.as_ref().unwrap();
    assert!(!id.to_string_lossy().starts_with('~'));
    assert!(id.ends_with(".ssh/foo"));
  }

  #[test]
  fn flags_existing_host_by_name() {
    let dir = TempDir::new().unwrap();
    let path = write_config(&dir, "Host already\n  HostName a.example.com\n  User u\n");

    let mut book = empty_book(&dir);
    book.add(Host {
      id: "x".into(),
      name: "already".into(),
      host: "irrelevant".into(),
      port: 22,
      user: "irrelevant".into(),
      auth: HostAuth::Password { in_keychain: false },
      proxy_jump: None,
    });
    let cands = parse_ssh_config(&path, &book).unwrap();
    assert!(cands[0].already_exists);
  }

  #[test]
  fn proxy_jump_is_propagated() {
    let dir = TempDir::new().unwrap();
    let path = write_config(
      &dir,
      "Host behind\n  HostName 10.0.0.1\n  User u\n  ProxyJump bastion\n",
    );
    let cands = parse_ssh_config(&path, &empty_book(&dir)).unwrap();
    assert!(cands[0].warnings.is_empty());
    assert_eq!(cands[0].proxy_jump.as_deref(), Some("bastion"));
  }
}
