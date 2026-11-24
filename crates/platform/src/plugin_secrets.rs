//! Plugin secrets, such as API keys the user typed for a plugin, live only in the OS
//! credential store. Plugins never read them; the extension host fills them into requests
//! for the origins a secret is declared for.
use crate::{ACCOUNT, CredentialError, entry, forget_entry};
use zeroize::Zeroizing;

/// Longest secret value accepted.
pub const MAX_SECRET_BYTES: usize = 4096;
/// Longest key accepted, such as `<scope>/<plugin>/<secret>`.
const MAX_KEY_BYTES: usize = 256;

fn name(key: &str) -> Result<String, CredentialError> {
	if key.is_empty()
		|| key.len() > MAX_KEY_BYTES
		|| !key
			.bytes()
			.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.'))
	{
		return Err(CredentialError::Invalid);
	}
	Ok(format!("{ACCOUNT}.plugin-secret.{key}"))
}

pub fn load(key: &str) -> Result<Option<Zeroizing<String>>, CredentialError> {
	match entry(&name(key)?)?.get_password() {
		Ok(value) if value.len() <= MAX_SECRET_BYTES => Ok(Some(Zeroizing::new(value))),
		Ok(value) => {
			drop(Zeroizing::new(value));
			Err(CredentialError::Invalid)
		}
		Err(keyring_core::Error::NoEntry) => Ok(None),
		Err(_) => Err(CredentialError::Unavailable),
	}
}

pub fn save(key: &str, value: &str) -> Result<(), CredentialError> {
	if value.is_empty() || value.len() > MAX_SECRET_BYTES || value.chars().any(char::is_control) {
		return Err(CredentialError::Invalid);
	}
	entry(&name(key)?)?
		.set_password(value)
		.map_err(|_| CredentialError::Unavailable)
}

pub fn forget(key: &str) -> Result<(), CredentialError> {
	forget_entry(&name(key)?)
}
