//! SSH transport for Elum.
//!
//! Owns a tokio runtime that drives `russh`. Exposes a small async API:
//! connect, exec, close. Future surface (out of scope for this spike):
//! interactive shell channels for the terminal, port forwarding, SFTP.
//!
//! No host-key verification policy yet, `accept_any_host_key` is hard-wired
//! to `true` in tests. Real builds will gate this on a `KnownHosts` store.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use russh::client::{self, Handle};
use russh::keys::{load_secret_key, ssh_key, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, Disconnect};

/// What a Russh client implements to react to server events. We only need
/// the host-key check; everything else uses defaults.
struct ClientHandler {
  accept_any_host_key: bool,
}

impl client::Handler for ClientHandler {
  type Error = russh::Error;

  async fn check_server_key(
    &mut self,
    _server_public_key: &ssh_key::PublicKey,
  ) -> Result<bool, Self::Error> {
    Ok(self.accept_any_host_key)
  }
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

#[derive(Debug, Clone)]
pub struct ConnectConfig {
  pub host: String,
  pub port: u16,
  pub user: String,
  pub auth: AuthMethod,
  /// Skip host-key validation. Currently always true while we don't have a
  /// known_hosts store; flagged so callers can flip it once we do.
  pub accept_any_host_key: bool,
}

impl ConnectConfig {
  pub fn new(
    host: impl Into<String>,
    port: u16,
    user: impl Into<String>,
    auth: AuthMethod,
  ) -> Self {
    Self {
      host: host.into(),
      port,
      user: user.into(),
      auth,
      accept_any_host_key: true,
    }
  }

  pub fn with_public_key(
    host: impl Into<String>,
    port: u16,
    user: impl Into<String>,
    key_path: impl AsRef<Path>,
  ) -> Self {
    Self::new(
      host,
      port,
      user,
      AuthMethod::PublicKey {
        key_path: key_path.as_ref().to_path_buf(),
        passphrase: None,
      },
    )
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
}

impl Session {
  /// Open a TCP+SSH connection and authenticate using the configured method.
  pub async fn connect(cfg: &ConnectConfig) -> Result<Self> {
    let config = Arc::new(client::Config::default());
    let handler = ClientHandler {
      accept_any_host_key: cfg.accept_any_host_key,
    };

    let mut handle = client::connect(config, (cfg.host.as_str(), cfg.port), handler)
      .await
      .with_context(|| format!("connecting to {}:{}", cfg.host, cfg.port))?;

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

    Ok(Self { handle })
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

  /// Open an interactive shell channel with a PTY. Returns a [`ShellHandle`]
  /// holding two flume channels: bytes coming from the remote (stdout +
  /// stderr) and bytes going to the remote (keystrokes). A background tokio
  /// task drives the bidirectional relay; dropping the handle does NOT
  /// stop the task by itself - call [`ShellHandle::close`] to shut down.
  ///
  /// Consumes `self` because the relay task takes ownership of the SSH
  /// session for the lifetime of the shell. Re-connect for a new session.
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

    // Move the session handle into the relay task so the SSH transport
    // stays alive for as long as the shell is in use.
    let task = tokio::spawn(relay_loop(
      self.handle,
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
