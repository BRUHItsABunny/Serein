//! Where plugin secrets are kept. The front end installs one store at startup: the OS
//! credential store on the desktop, memory in the demo. The host reads a secret only on the
//! extension worker, to fill it into a request; plugins never receive it.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};

use zeroize::Zeroizing;

/// Longest secret value accepted.
pub const MAX_SECRET_BYTES: usize = 4096;

pub trait SecretStore: Send + Sync {
	fn load(&self, key: &str) -> Result<Option<Zeroizing<String>>, String>;
	fn save(&self, key: &str, value: &str) -> Result<(), String>;
	fn forget(&self, key: &str) -> Result<(), String>;
}

static STORE: OnceLock<Arc<dyn SecretStore>> = OnceLock::new();

/// Installs the process's secret store; later calls keep the first one.
pub fn install(store: Arc<dyn SecretStore>) {
	let _ = STORE.set(store);
}

pub(crate) fn store() -> Result<&'static Arc<dyn SecretStore>, String> {
	STORE
		.get()
		.ok_or_else(|| "Plugin secrets are unavailable on this device".to_string())
}

/// Credential-store key of one secret: `<scope>/<plugin>/<secret>`, where the scope is the
/// account's key (a hash, never the account ID) or `connection`.
pub(crate) fn key(scope: &str, plugin: &str, secret: &str) -> String {
	format!("{scope}/{plugin}/{secret}")
}

/// Accepts what a user may paste as a secret: 1 to 4,096 bytes without control characters,
/// so a value can never split a header.
pub fn valid_value(value: &str) -> bool {
	!value.is_empty() && value.len() <= MAX_SECRET_BYTES && !value.chars().any(char::is_control)
}

/// Keeps secrets in memory for the process; for the demo and tests.
#[derive(Default)]
pub struct MemoryStore(Mutex<BTreeMap<String, Zeroizing<String>>>);

impl SecretStore for MemoryStore {
	fn load(&self, key: &str) -> Result<Option<Zeroizing<String>>, String> {
		let values = self.0.lock().map_err(|_| "Secret store unavailable")?;
		Ok(values.get(key).cloned())
	}
	fn save(&self, key: &str, value: &str) -> Result<(), String> {
		let mut values = self.0.lock().map_err(|_| "Secret store unavailable")?;
		values.insert(key.to_owned(), Zeroizing::new(value.to_owned()));
		Ok(())
	}
	fn forget(&self, key: &str) -> Result<(), String> {
		let mut values = self.0.lock().map_err(|_| "Secret store unavailable")?;
		values.remove(key);
		Ok(())
	}
}
