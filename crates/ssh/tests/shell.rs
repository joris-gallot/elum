//! Integration tests for the interactive shell path.
//!
//! Requires the local SSH test container to be running:
//!     docker compose -f docker/sshd/compose.yml up -d

use std::path::PathBuf;
use std::time::Duration;

use std::sync::Arc;

use ssh::{AlwaysAccept, AuthMethod, ConnectConfig, Session};
use tokio::time::timeout;

fn test_key_path() -> PathBuf {
  let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo");
  PathBuf::from(manifest_dir)
    .join("..")
    .join("..")
    .join("docker")
    .join("sshd")
    .join("fixtures")
    .join("id_ed25519")
}

fn test_config() -> ConnectConfig {
  ConnectConfig::new(
    "127.0.0.1",
    2222,
    "testuser",
    AuthMethod::PublicKey {
      key_path: test_key_path(),
      passphrase: None,
    },
    Arc::new(AlwaysAccept),
  )
}

/// Drain bytes from the shell until `needle` appears or the deadline elapses.
/// Returns the accumulated buffer (including the needle if found).
async fn read_until(rx: &flume::Receiver<Vec<u8>>, needle: &[u8], wait: Duration) -> Vec<u8> {
  let mut buf = Vec::new();
  let deadline = tokio::time::Instant::now() + wait;
  loop {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
      break;
    }
    match timeout(remaining, rx.recv_async()).await {
      Ok(Ok(chunk)) => {
        buf.extend_from_slice(&chunk);
        if buf.windows(needle.len()).any(|w| w == needle) {
          break;
        }
      }
      Ok(Err(_)) | Err(_) => break,
    }
  }
  buf
}

#[tokio::test]
async fn open_shell_emits_a_prompt() {
  let session = Session::connect(&test_config()).await.expect("connect");
  let shell = session.open_shell(80, 24).await.expect("open shell");

  // Bash prints a prompt within the first chunks. We don't know the exact
  // prompt string, but `$` is a reliable end-of-prompt marker for the
  // default Alpine `bash` config.
  let buf = read_until(&shell.from_remote, b"$", Duration::from_secs(3)).await;
  assert!(!buf.is_empty(), "expected some shell output, got nothing");
  assert!(
    buf.windows(1).any(|w| w == b"$"),
    "expected a `$` somewhere in the prompt; got: {:?}",
    String::from_utf8_lossy(&buf)
  );

  shell.close().await;
}

#[tokio::test]
async fn shell_echoes_typed_command_output() {
  let session = Session::connect(&test_config()).await.expect("connect");
  let shell = session.open_shell(80, 24).await.expect("open shell");

  // Wait for the initial prompt so we don't race the shell startup.
  let _prompt = read_until(&shell.from_remote, b"$", Duration::from_secs(3)).await;

  // Send a command. `\n` should be enough - bash treats it like Enter.
  shell
    .to_remote
    .send(b"echo from_shell\n".to_vec())
    .expect("send");

  // The shell echoes our input AND the command output. We just need
  // `from_shell` to appear somewhere in the byte stream.
  let buf = read_until(&shell.from_remote, b"from_shell", Duration::from_secs(3)).await;
  assert!(
    buf.windows(b"from_shell".len()).any(|w| w == b"from_shell"),
    "expected `from_shell` in output, got: {:?}",
    String::from_utf8_lossy(&buf)
  );

  shell.close().await;
}

#[tokio::test]
async fn resize_does_not_break_the_relay() {
  let session = Session::connect(&test_config()).await.expect("connect");
  let shell = session.open_shell(80, 24).await.expect("open shell");

  // Wait for prompt, then resize. The remote shouldn't crash.
  let _ = read_until(&shell.from_remote, b"$", Duration::from_secs(3)).await;

  shell.resize.send((120, 40)).expect("send resize");

  // Issue a command after resize to confirm the relay still works.
  shell
    .to_remote
    .send(b"echo after_resize\n".to_vec())
    .expect("send");
  let buf = read_until(&shell.from_remote, b"after_resize", Duration::from_secs(3)).await;
  assert!(
    buf
      .windows(b"after_resize".len())
      .any(|w| w == b"after_resize"),
    "expected `after_resize` after resize; got: {:?}",
    String::from_utf8_lossy(&buf)
  );

  shell.close().await;
}
