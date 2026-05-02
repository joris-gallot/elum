//! Integration tests for the SSH transport.
//!
//! Requires the local SSH test container to be running. Bring it up with:
//!
//!     docker compose -f docker/sshd/compose.yml up -d
//!
//! and tear it down with:
//!
//!     docker compose -f docker/sshd/compose.yml down

use std::path::PathBuf;

use ssh::{ConnectConfig, Session};

/// Path to the test private key, relative to this crate's manifest dir.
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
  ConnectConfig::with_public_key("127.0.0.1", 2222, "testuser", test_key_path())
}

#[tokio::test]
async fn connects_and_authenticates() {
  let session = Session::connect(&test_config())
    .await
    .expect("connect to local sshd container - is `docker compose up` running?");
  session.close().await.expect("clean disconnect");
}

#[tokio::test]
async fn exec_echo_returns_stdout() {
  let session = Session::connect(&test_config())
    .await
    .expect("connect to local sshd container");

  let out = session.exec("echo hello").await.expect("exec echo command");

  assert_eq!(out.exit_status, 0);
  assert_eq!(out.stdout, b"hello\n");
  assert!(out.stderr.is_empty());

  session.close().await.ok();
}

#[tokio::test]
async fn exec_whoami_returns_testuser() {
  let session = Session::connect(&test_config())
    .await
    .expect("connect to local sshd container");

  let out = session.exec("whoami").await.expect("exec whoami");

  assert_eq!(out.exit_status, 0);
  assert_eq!(out.stdout, b"testuser\n");

  session.close().await.ok();
}

#[tokio::test]
async fn exec_failing_command_reports_nonzero_exit_and_stderr() {
  let session = Session::connect(&test_config())
    .await
    .expect("connect to local sshd container");

  // `false` always exits 1 with no output. Pair it with a stderr writer
  // for a fuller assertion.
  let out = session
    .exec("sh -c 'echo oops 1>&2; exit 7'")
    .await
    .expect("exec failing command");

  assert_eq!(out.exit_status, 7);
  assert!(out.stdout.is_empty());
  assert_eq!(out.stderr, b"oops\n");

  session.close().await.ok();
}

#[tokio::test]
async fn exec_can_be_called_multiple_times_on_one_session() {
  let session = Session::connect(&test_config())
    .await
    .expect("connect to local sshd container");

  let a = session.exec("echo first").await.unwrap();
  let b = session.exec("echo second").await.unwrap();

  assert_eq!(a.stdout, b"first\n");
  assert_eq!(b.stdout, b"second\n");

  session.close().await.ok();
}
