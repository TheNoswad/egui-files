//! What's kept so as not to sign in each time, in the OS's keyring: Secret
//! Service on Linux, Credential Manager on Windows.
//!
//! Never call these on the async runtime: the stores block.

use std::sync::OnceLock;

use keyring_core::{Entry, Error};
use proton_sdk::{
    account::KeyPassphrases,
    http::Tokens,
    session::{PasswordMode, ProtonApiSession},
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

const SERVICE: &str = "egui-files";
const ACCOUNT: &str = "proton-drive";

/// Everything needed to pick the session up again. As good as the password
/// for reading the Drive, so it has no `Debug` and goes nowhere but the
/// keyring.
#[derive(Clone, Deserialize, Serialize)]
pub struct Saved {
    pub session_id: String,
    pub username: String,
    pub user_id: String,
    pub access_token: String,
    pub refresh_token: String,
    pub scopes: Vec<String>,
    pub dual_password: bool,
    /// Key id and its passphrase, in hex.
    keys: Vec<(String, String)>,
}

impl Saved {
    pub fn new(session: &ProtonApiSession, tokens: &Tokens, keys: &KeyPassphrases) -> Self {
        Self {
            session_id: session.session_id().to_string(),
            username: session.username().to_owned(),
            user_id: session.user_id().to_string(),
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            scopes: session.scopes().to_vec(),
            dual_password: session.password_mode() == PasswordMode::Dual,
            keys: keys
                .iter()
                .map(|(id, passphrase)| (id.to_owned(), to_hex(passphrase)))
                .collect(),
        }
    }

    pub fn passphrases(&self) -> Result<KeyPassphrases, &'static str> {
        let mut keys = KeyPassphrases::new();
        for (id, hex) in &self.keys {
            keys.insert(
                id.clone(),
                from_hex(hex).ok_or("the saved sign-in is damaged")?,
            );
        }
        Ok(keys)
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&text[at..at + 2], 16).ok())
        .collect()
}

/// The keyring's entry for the account, the store being set up first.
fn entry() -> Result<Entry, String> {
    static STORE: OnceLock<Result<(), String>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            let store = zbus_secret_service_keyring_store::Store::new();
            #[cfg(windows)]
            let store = windows_native_keyring_store::Store::new();
            keyring_core::set_default_store(store.map_err(|err| err.to_string())?);
            Ok(())
        })
        .clone()?;
    Entry::new(SERVICE, ACCOUNT).map_err(|err| err.to_string())
}

pub fn save(saved: &Saved) -> Result<(), String> {
    let json = Zeroizing::new(serde_json::to_vec(saved).map_err(|err| err.to_string())?);
    entry()?.set_secret(&json).map_err(|err| err.to_string())
}

/// What's saved, if anything.
pub fn load() -> Result<Option<Saved>, String> {
    match entry()?.get_secret() {
        Ok(json) => {
            let json = Zeroizing::new(json);
            serde_json::from_slice(&json)
                .map(Some)
                .map_err(|_| "the saved sign-in is damaged".to_owned())
        }
        Err(Error::NoEntry) => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

pub fn delete() -> Result<(), String> {
    match entry()?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(err) => Err(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 127, 128, 255];
        assert_eq!(to_hex(&bytes), "00017f80ff");
        assert_eq!(from_hex("00017f80ff").as_deref(), Some(&bytes[..]));
        assert_eq!(from_hex("0"), None);
        assert_eq!(from_hex("zz"), None);
        assert_eq!(from_hex("é1"), None);
    }

    #[test]
    fn saved_round_trips_with_its_passphrases() {
        let saved = Saved {
            session_id: "s".into(),
            username: "u".into(),
            user_id: "i".into(),
            access_token: "a".into(),
            refresh_token: "r".into(),
            scopes: vec!["full".into()],
            dual_password: true,
            keys: vec![("key".into(), to_hex(b"secret"))],
        };
        let json = serde_json::to_vec(&saved).unwrap();
        let back: Saved = serde_json::from_slice(&json).unwrap();
        let expected = KeyPassphrases::from_iter([("key", b"secret".to_vec())]);
        assert!(back.passphrases().unwrap() == expected);
        assert!(back.dual_password);
    }
}
