use anyhow::Result;
use keyring::{Entry, Error};

const SERVICE_PREFIX: &str = "com.elum.host.";
const ACCOUNT: &str = "passphrase";

fn entry(host_id: &str) -> Result<Entry> {
  let service = format!("{SERVICE_PREFIX}{host_id}");
  Entry::new(&service, ACCOUNT).map_err(Into::into)
}

pub fn store(host_id: &str, passphrase: &str) -> Result<()> {
  entry(host_id)?.set_password(passphrase)?;
  Ok(())
}

pub fn fetch(host_id: &str) -> Option<String> {
  let entry = entry(host_id).ok()?;
  match entry.get_password() {
    Ok(p) => Some(p),
    Err(Error::NoEntry) => None,
    Err(e) => {
      eprintln!("warning: keychain lookup for {host_id} failed: {e}");
      None
    }
  }
}

pub fn delete(host_id: &str) {
  let Ok(entry) = entry(host_id) else {
    return;
  };
  match entry.delete_credential() {
    Ok(()) | Err(Error::NoEntry) => {}
    Err(e) => eprintln!("warning: keychain delete for {host_id} failed: {e}"),
  }
}
