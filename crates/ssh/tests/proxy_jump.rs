//! Requires `docker compose -f docker/sshd/compose.yml up -d`.
//! Container loops through itself for the inner hop: `localhost:22` is
//! resolved by the outer sshd, so it lands back on the same daemon.

use std::path::PathBuf;
use std::sync::Arc;

use ssh::{AlwaysAccept, AuthMethod, ConnectConfig, Session};

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

fn key_auth() -> AuthMethod {
  AuthMethod::PublicKey {
    key_path: test_key_path(),
    passphrase: None,
  }
}

fn outer_config() -> ConnectConfig {
  ConnectConfig::new(
    "127.0.0.1",
    2222,
    "testuser",
    key_auth(),
    Arc::new(AlwaysAccept),
  )
}

fn inner_config() -> ConnectConfig {
  ConnectConfig::new(
    "localhost",
    22,
    "testuser",
    key_auth(),
    Arc::new(AlwaysAccept),
  )
}

#[tokio::test]
async fn connect_via_authenticates_through_jump_host() {
  let jump = Session::connect(&outer_config())
    .await
    .expect("connect to local sshd container - is `docker compose up` running?");

  let target = Session::connect_via(jump, &inner_config())
    .await
    .expect("ssh handshake over the jump hop");

  target.close().await.expect("clean disconnect");
}

#[tokio::test]
async fn exec_over_proxy_jump_returns_stdout() {
  let jump = Session::connect(&outer_config())
    .await
    .expect("connect to local sshd container");

  let target = Session::connect_via(jump, &inner_config())
    .await
    .expect("ssh handshake over jump");

  let out = target.exec("echo via-jump").await.expect("exec");
  assert_eq!(out.exit_status, 0);
  assert_eq!(out.stdout, b"via-jump\n");

  target.close().await.ok();
}

#[tokio::test]
async fn nested_proxy_jump_two_hops() {
  let hop1 = Session::connect(&outer_config())
    .await
    .expect("connect to local sshd container");
  let hop2 = Session::connect_via(hop1, &inner_config())
    .await
    .expect("first hop");
  let target = Session::connect_via(hop2, &inner_config())
    .await
    .expect("second hop");

  let out = target.exec("whoami").await.expect("exec");
  assert_eq!(out.stdout, b"testuser\n");

  target.close().await.ok();
}
