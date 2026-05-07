//! SSH transport over `russh`. Host-key verification is delegated to [`HostKeyPolicy`].

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::known_hosts::{check_known_hosts_path, learn_known_hosts_path};
use russh::keys::{load_secret_key, ssh_key, Error as KeysError, HashAlg, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};

/// Outcome of consulting `known_hosts` for the current connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyStatus {
  /// `host:port` is not present in `known_hosts`
  New,
  /// `host:port` is present but the key advertised now differs from the one recorded earlier
  Changed { previous_line: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostKeyVerdict {
  /// Connect this once, do not record the key.
  AcceptOnce,
  /// Connect and remember (writes / replaces the entry in `known_hosts`).
  AcceptAndRemember,
  /// Refuse the connection.
  Reject,
}

/// Information passed to the policy when prompting the user
#[derive(Debug, Clone)]
pub struct HostKeyInfo {
  pub host: String,
  pub port: u16,
  pub status: HostKeyStatus,
  pub key_algorithm: String,
  pub fingerprint: String,
}

#[async_trait]
pub trait HostKeyPolicy: Send + Sync {
  async fn verify(&self, info: HostKeyInfo) -> HostKeyVerdict;
}

/// Test/CLI fixture that auto-accepts every host key without prompting.
/// Never use in production code paths.
pub struct AlwaysAccept;

#[async_trait]
impl HostKeyPolicy for AlwaysAccept {
  async fn verify(&self, _info: HostKeyInfo) -> HostKeyVerdict {
    HostKeyVerdict::AcceptOnce
  }
}

/// Bridges russh's `client::Handler` to our `HostKeyPolicy`.
struct ClientHandler {
  host: String,
  port: u16,
  policy: Arc<dyn HostKeyPolicy>,
  known_hosts_path: PathBuf,
}

impl client::Handler for ClientHandler {
  type Error = anyhow::Error;

  async fn check_server_key(
    &mut self,
    server_public_key: &ssh_key::PublicKey,
  ) -> Result<bool, Self::Error> {
    let status = match check_known_hosts_path(
      &self.host,
      self.port,
      server_public_key,
      &self.known_hosts_path,
    ) {
      Ok(true) => return Ok(true),
      Ok(false) => HostKeyStatus::New,
      Err(KeysError::KeyChanged { line }) => HostKeyStatus::Changed {
        previous_line: line,
      },
      Err(e) => return Err(anyhow!("known_hosts check failed: {e}")),
    };

    let info = HostKeyInfo {
      host: self.host.clone(),
      port: self.port,
      status,
      key_algorithm: server_public_key.algorithm().to_string(),
      fingerprint: server_public_key.fingerprint(HashAlg::Sha256).to_string(),
    };

    let verdict = self.policy.verify(info).await;
    match verdict {
      HostKeyVerdict::Reject => Ok(false),
      HostKeyVerdict::AcceptOnce => Ok(true),
      HostKeyVerdict::AcceptAndRemember => {
        if let HostKeyStatus::Changed { previous_line } = status {
          remove_known_hosts_line(&self.known_hosts_path, previous_line).with_context(|| {
            format!(
              "removing stale entry from {}",
              self.known_hosts_path.display()
            )
          })?;
        }
        learn_known_hosts_path(
          &self.host,
          self.port,
          server_public_key,
          &self.known_hosts_path,
        )
        .with_context(|| format!("writing {}", self.known_hosts_path.display()))?;
        Ok(true)
      }
    }
  }
}

/// Resolve the user's `~/.ssh/known_hosts`
/// Falls back to the current directory if no home dir can be found
pub fn default_known_hosts_path() -> PathBuf {
  dirs::home_dir()
    .unwrap_or_else(|| PathBuf::from("."))
    .join(".ssh")
    .join("known_hosts")
}

/// Delete a single line (1-based, matching what russh reports in
/// [`HostKeyStatus::Changed`]) from `known_hosts`
/// No-op if the file doesn't exist or the line is out of range.
fn remove_known_hosts_line(path: &Path, line_1based: usize) -> std::io::Result<()> {
  if !path.exists() {
    return Ok(());
  }
  let contents = std::fs::read_to_string(path)?;
  let mut lines: Vec<&str> = contents.lines().collect();
  if line_1based == 0 || line_1based > lines.len() {
    return Ok(());
  }
  lines.remove(line_1based - 1);
  let mut out = lines.join("\n");
  if !out.is_empty() {
    out.push('\n');
  }
  std::fs::write(path, out)
}

#[derive(Debug, Clone)]
pub enum AuthMethod {
  PublicKey {
    key_path: std::path::PathBuf,
    passphrase: Option<String>,
  },
  Password {
    password: String,
  },
}

#[derive(Clone)]
pub struct ConnectConfig {
  pub host: String,
  pub port: u16,
  pub user: String,
  pub auth: AuthMethod,
  /// Decides what to do when the server's key isn't already trusted.
  pub host_key_policy: Arc<dyn HostKeyPolicy>,
  /// Path to the `known_hosts` file. Defaults to `~/.ssh/known_hosts` so
  /// trust decisions interop with the system `ssh` CLI; tests override
  /// it to a temp file.
  pub known_hosts_path: PathBuf,
}

impl ConnectConfig {
  pub fn new(
    host: impl Into<String>,
    port: u16,
    user: impl Into<String>,
    auth: AuthMethod,
    host_key_policy: Arc<dyn HostKeyPolicy>,
  ) -> Self {
    Self {
      host: host.into(),
      port,
      user: user.into(),
      auth,
      host_key_policy,
      known_hosts_path: default_known_hosts_path(),
    }
  }
}

/// Result of a one-shot remote command execution.
#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
  pub stdout: Vec<u8>,
  pub stderr: Vec<u8>,
  pub exit_status: u32,
}

/// An authenticated SSH session. Keep this alive as long as you want to
/// issue commands on it; drop or `close()` it when done.
pub struct Session {
  handle: Handle<ClientHandler>,
  /// Tunnel chain kept alive for the lifetime of this session.
  /// Innermost-first: `parents[0]` is this session's direct ProxyJump host.
  parents: Vec<Session>,
}

impl Session {
  /// Open a TCP+SSH connection and authenticate using the configured method.
  pub async fn connect(cfg: &ConnectConfig) -> Result<Self> {
    let config = Arc::new(client::Config::default());
    let handler = Self::build_handler(cfg);

    let handle = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
      .await
      .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;

    Self::finish_handshake(handle, cfg, Vec::new()).await
  }

  /// Open an SSH session tunneled through `parent` (ProxyJump). The parent
  /// keeps its transport alive for as long as the returned session lives.
  pub async fn connect_via(parent: Session, cfg: &ConnectConfig) -> Result<Self> {
    let channel = parent
      .handle
      .channel_open_direct_tcpip(cfg.host.clone(), u32::from(cfg.port), "", 0)
      .await
      .with_context(|| {
        format!(
          "opening direct-tcpip to {}:{} via jump host",
          cfg.host, cfg.port
        )
      })?;
    let stream = channel.into_stream();

    let config = Arc::new(client::Config::default());
    let handler = Self::build_handler(cfg);
    let handle = client::connect_stream(config, stream, handler)
      .await
      .with_context(|| format!("ssh handshake to {}:{} via jump host", cfg.host, cfg.port))?;

    Self::finish_handshake(handle, cfg, vec![parent]).await
  }

  fn build_handler(cfg: &ConnectConfig) -> ClientHandler {
    ClientHandler {
      host: cfg.host.clone(),
      port: cfg.port,
      policy: cfg.host_key_policy.clone(),
      known_hosts_path: cfg.known_hosts_path.clone(),
    }
  }

  async fn finish_handshake(
    mut handle: Handle<ClientHandler>,
    cfg: &ConnectConfig,
    parents: Vec<Session>,
  ) -> Result<Self> {
    let auth = match &cfg.auth {
      AuthMethod::PublicKey {
        key_path,
        passphrase,
      } => {
        let key = load_secret_key(key_path, passphrase.as_deref())
          .with_context(|| format!("loading key from {}", key_path.display()))?;
        // RSA hash negotiation only matters for RSA keys; Ed25519/ECDSA pass
        // `None`. Asking the server is cheap and handles all key types.
        let rsa_hash = handle.best_supported_rsa_hash().await?.flatten();
        handle
          .authenticate_publickey(
            &cfg.user,
            PrivateKeyWithHashAlg::new(Arc::new(key), rsa_hash),
          )
          .await
          .context("public-key authentication failed")?
      }
      AuthMethod::Password { password } => handle
        .authenticate_password(&cfg.user, password)
        .await
        .context("password authentication failed")?,
    };

    if !auth.success() {
      return Err(anyhow!("authentication rejected for user `{}`", cfg.user));
    }

    Ok(Self { handle, parents })
  }

  /// Execute a single non-interactive command and collect all output. The
  /// channel is opened, exec'd, drained, and closed before returning.
  /// Use this for `whoami`, `ls`, etc. - not for interactive shells.
  pub async fn exec(&self, command: &str) -> Result<ExecOutput> {
    let mut channel = self
      .handle
      .channel_open_session()
      .await
      .context("opening session channel")?;

    channel
      .exec(true, command)
      .await
      .with_context(|| format!("exec(`{command}`) failed"))?;

    let mut output = ExecOutput::default();

    // Drain until the channel reports no more messages. We deliberately
    // do NOT break on `Eof`/`Close` - some sshd implementations send
    // `exit-status` AFTER EOF, and an early break loses the exit code.
    while let Some(msg) = channel.wait().await {
      match msg {
        ChannelMsg::Data { ref data } => output.stdout.extend_from_slice(data),
        ChannelMsg::ExtendedData { ref data, ext: 1 } => {
          output.stderr.extend_from_slice(data);
        }
        ChannelMsg::ExitStatus { exit_status } => output.exit_status = exit_status,
        _ => {}
      }
    }

    channel.close().await.ok();
    Ok(output)
  }

  /// Politely close the underlying SSH transport. Safe to call once.
  pub async fn close(self) -> Result<()> {
    self
      .handle
      .disconnect(Disconnect::ByApplication, "", "")
      .await
      .context("disconnect")?;
    Ok(())
  }

  /// Open an interactive PTY shell. Consumes `self` because the relay task
  /// owns the session for its lifetime; call [`ShellHandle::close`] to shut down.
  pub async fn open_shell(self, cols: u16, rows: u16) -> Result<ShellHandle> {
    let channel = self
      .handle
      .channel_open_session()
      .await
      .context("opening shell channel")?;

    channel
      .request_pty(true, "xterm-256color", cols as u32, rows as u32, 0, 0, &[])
      .await
      .context("request_pty failed")?;

    channel
      .request_shell(true)
      .await
      .context("request_shell failed")?;

    let (from_remote_tx, from_remote_rx) = flume::unbounded::<Vec<u8>>();
    let (to_remote_tx, to_remote_rx) = flume::unbounded::<Vec<u8>>();
    let (resize_tx, resize_rx) = flume::unbounded::<(u16, u16)>();

    // Move both the session handle and any tunneling parents into the relay
    // task so the entire SSH chain stays alive for as long as the shell is
    // in use.
    let task = tokio::spawn(relay_loop(
      self.handle,
      self.parents,
      channel,
      from_remote_tx,
      to_remote_rx,
      resize_rx,
    ));

    Ok(ShellHandle {
      from_remote: from_remote_rx,
      to_remote: to_remote_tx,
      resize: resize_tx,
      task: Some(task),
    })
  }
}

/// Bidirectional relay: SSH channel ↔ flume channels. Runs on a tokio task.
/// Exits when the SSH side closes, the to-remote sender is dropped, or any
/// SSH operation errors.
async fn relay_loop(
  _handle: Handle<ClientHandler>,
  _parents: Vec<Session>,
  mut channel: russh::Channel<russh::client::Msg>,
  from_remote: flume::Sender<Vec<u8>>,
  to_remote: flume::Receiver<Vec<u8>>,
  resize: flume::Receiver<(u16, u16)>,
) {
  loop {
    tokio::select! {
        msg = channel.wait() => {
            let Some(msg) = msg else { break };
            let bytes_for_view = match msg {
                ChannelMsg::Data { ref data }
                | ChannelMsg::ExtendedData { ref data, ext: 1 } => Some(data.to_vec()),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => None,
            };
            if let Some(bytes) = bytes_for_view {
                if from_remote.send(bytes).is_err() {
                    break;
                }
            }
        }
        res = to_remote.recv_async() => {
            let Ok(bytes) = res else { break };
            if channel.data(&bytes[..]).await.is_err() {
                break;
            }
        }
        res = resize.recv_async() => {
            let Ok((cols, rows)) = res else { break };
            let _ = channel.window_change(cols as u32, rows as u32, 0, 0).await;
        }
    }
  }

  // Best-effort cleanup. Errors here usually mean the channel is already
  // gone, which is fine.
  let _ = channel.eof().await;
  let _ = channel.close().await;
}

/// Active interactive shell. Holds the channels for byte-level I/O and a
/// resize signal. Drop this OR call [`ShellHandle::close`] to terminate.
pub struct ShellHandle {
  /// Bytes streaming from the remote (stdout + stderr merged).
  pub from_remote: flume::Receiver<Vec<u8>>,
  /// Bytes to send to the remote (typically keystrokes).
  pub to_remote: flume::Sender<Vec<u8>>,
  /// Window resize signals: `(cols, rows)`.
  pub resize: flume::Sender<(u16, u16)>,
  task: Option<tokio::task::JoinHandle<()>>,
}

impl ShellHandle {
  /// Stop the relay task and wait for it to exit. Idempotent.
  pub async fn close(mut self) {
    // Dropping the senders signals the relay loop to exit on the next
    // recv_async.
    drop(std::mem::replace(
      &mut self.from_remote,
      flume::unbounded().1,
    ));
    if let Some(task) = self.task.take() {
      task.abort();
      let _ = task.await;
    }
  }
}

impl Drop for ShellHandle {
  fn drop(&mut self) {
    if let Some(task) = self.task.take() {
      task.abort();
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use russh::client::Handler as _;
  use ssh_key::PublicKey;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use std::sync::Mutex;
  use tempfile::TempDir;

  // No OpenSSH comment: `PublicKey`'s `PartialEq` includes it, and wire keys carry none.
  const KEY_A: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFpA9qS29zSWIfEGh5E5CnHzVpQPnSQq5fOuRWE6tLeu";
  const KEY_B: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGKtk3wKgiNr8wO+06/gnHBkDgtTYUyw3h3JAPwwzhzf";

  fn pubkey(s: &str) -> PublicKey {
    PublicKey::from_openssh(s).expect("parse test key")
  }

  /// Tracks calls and returns a fixed verdict so tests pin behavior
  /// without spinning a UI dialog.
  struct StubPolicy {
    verdict: HostKeyVerdict,
    seen_status: Arc<Mutex<Option<HostKeyStatus>>>,
    calls: Arc<AtomicUsize>,
  }

  impl StubPolicy {
    fn new(
      verdict: HostKeyVerdict,
    ) -> (
      Arc<Self>,
      Arc<Mutex<Option<HostKeyStatus>>>,
      Arc<AtomicUsize>,
    ) {
      let seen = Arc::new(Mutex::new(None));
      let calls = Arc::new(AtomicUsize::new(0));
      let policy = Arc::new(Self {
        verdict,
        seen_status: seen.clone(),
        calls: calls.clone(),
      });
      (policy, seen, calls)
    }
  }

  #[async_trait]
  impl HostKeyPolicy for StubPolicy {
    async fn verify(&self, info: HostKeyInfo) -> HostKeyVerdict {
      *self.seen_status.lock().unwrap() = Some(info.status);
      self.calls.fetch_add(1, Ordering::SeqCst);
      self.verdict
    }
  }

  fn handler_at(path: PathBuf, policy: Arc<dyn HostKeyPolicy>) -> ClientHandler {
    ClientHandler {
      host: "test-host".into(),
      port: 22,
      policy,
      known_hosts_path: path,
    }
  }

  #[tokio::test]
  async fn missing_known_hosts_treats_host_as_new_and_consults_policy() {
    let dir = TempDir::new().unwrap();
    let (policy, seen, calls) = StubPolicy::new(HostKeyVerdict::AcceptOnce);
    let mut handler = handler_at(dir.path().join("known_hosts"), policy);

    let accepted = handler.check_server_key(&pubkey(KEY_A)).await.unwrap();
    assert!(accepted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(*seen.lock().unwrap(), Some(HostKeyStatus::New));
  }

  #[tokio::test]
  async fn matching_recorded_key_skips_policy_entirely() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("known_hosts");
    learn_known_hosts_path("test-host", 22, &pubkey(KEY_A), &path).unwrap();

    let (policy, _, calls) = StubPolicy::new(HostKeyVerdict::Reject);
    let mut handler = handler_at(path, policy);

    assert!(handler.check_server_key(&pubkey(KEY_A)).await.unwrap());
    assert_eq!(
      calls.load(Ordering::SeqCst),
      0,
      "policy must not be consulted for an already-trusted key"
    );
  }

  #[tokio::test]
  async fn changed_key_is_reported_to_policy() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("known_hosts");
    learn_known_hosts_path("test-host", 22, &pubkey(KEY_A), &path).unwrap();

    let (policy, seen, _) = StubPolicy::new(HostKeyVerdict::Reject);
    let mut handler = handler_at(path, policy);

    let accepted = handler.check_server_key(&pubkey(KEY_B)).await.unwrap();
    assert!(!accepted, "Reject must close the connection");
    assert!(matches!(
      *seen.lock().unwrap(),
      Some(HostKeyStatus::Changed { .. })
    ));
  }

  #[tokio::test]
  async fn accept_and_remember_writes_new_host_to_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("known_hosts");
    let (policy, _, _) = StubPolicy::new(HostKeyVerdict::AcceptAndRemember);
    let mut handler = handler_at(path.clone(), policy);

    handler.check_server_key(&pubkey(KEY_A)).await.unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    assert!(contents.contains("test-host"));
    assert!(
      contents.contains("AAAAC3NzaC1lZDI1NTE5AAAAIFpA9qS29zSWIfEGh5E5CnHzVpQPnSQq5fOuRWE6tLeu")
    );
  }

  #[tokio::test]
  async fn accept_and_remember_replaces_changed_entry() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("known_hosts");
    learn_known_hosts_path("test-host", 22, &pubkey(KEY_A), &path).unwrap();

    let (policy, _, _) = StubPolicy::new(HostKeyVerdict::AcceptAndRemember);
    let mut handler = handler_at(path.clone(), policy);

    handler.check_server_key(&pubkey(KEY_B)).await.unwrap();

    let contents = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "stale entry should be removed");
    assert!(
      contents.contains("AAAAC3NzaC1lZDI1NTE5AAAAIGKtk3wKgiNr8wO+06/gnHBkDgtTYUyw3h3JAPwwzhzf")
    );
  }

  #[test]
  fn remove_known_hosts_line_drops_target_only() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("known_hosts");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

    remove_known_hosts_line(&path, 2).unwrap();

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\ngamma\n");
  }

  #[test]
  fn remove_known_hosts_line_out_of_range_is_a_noop() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("known_hosts");
    std::fs::write(&path, "only\n").unwrap();
    remove_known_hosts_line(&path, 99).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "only\n");
  }
}
