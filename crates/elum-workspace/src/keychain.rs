use anyhow::Result;
use keyring::{Entry, Error};

const SERVICE_PREFIX: &str = "com.elum.host.";

pub const PASSPHRASE: &str = "passphrase";
pub const PASSWORD: &str = "password";

fn entry(host_id: &str, account: &str) -> Result<Entry> {
  let service = format!("{SERVICE_PREFIX}{host_id}");
  Entry::new(&service, account).map_err(Into::into)
}

pub fn store(host_id: &str, account: &str, secret: &str) -> Result<()> {
  entry(host_id, account)?.set_password(secret)?;
  Ok(())
}

pub fn fetch(host_id: &str, account: &str) -> Option<String> {
  let entry = entry(host_id, account).ok()?;
  match entry.get_password() {
    Ok(p) => Some(p),
    Err(Error::NoEntry) => None,
    Err(e) => {
      eprintln!("warning: keychain lookup for {host_id}/{account} failed: {e}");
      None
    }
  }
}

pub fn delete(host_id: &str, account: &str) {
  let Ok(entry) = entry(host_id, account) else {
    return;
  };
  match entry.delete_credential() {
    Ok(()) | Err(Error::NoEntry) => {}
    Err(e) => eprintln!("warning: keychain delete for {host_id}/{account} failed: {e}"),
  }
}
