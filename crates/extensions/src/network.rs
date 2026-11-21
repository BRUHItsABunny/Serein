//! Origins a plugin may reach with `serein.https_request`, declared in its manifest.
//!
//! A manifest's `network` list names exact HTTPS origins such as `https://api.example.org`
//! or `https://api.example.org:8443`. There are no wildcards, paths or IP literals: the user
//! approves every host by name at install, and the host resolves each one and refuses
//! private addresses itself.

/// Origins one manifest may declare.
pub const MAX_ORIGINS: usize = 16;

/// A validated HTTPS origin.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
	/// Lowercase DNS name with at least one dot.
	pub host: String,
	pub port: u16,
}

impl Origin {
	/// Parses `https://host[:port][/]`, rejecting anything else: other schemes, credentials,
	/// paths, queries, fragments, IP literals and single-label names.
	pub fn parse(text: &str) -> Option<Self> {
		if text.len() > 256 {
			return None;
		}
		let url = url::Url::parse(text).ok()?;
		let host = match url.host()? {
			url::Host::Domain(host) => host.to_ascii_lowercase(),
			url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
		};
		if url.scheme() != "https"
			|| !url.username().is_empty()
			|| url.password().is_some()
			|| url.path() != "/"
			|| url.query().is_some()
			|| url.fragment().is_some()
			|| !host.contains('.')
			|| !host
				.bytes()
				.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
			|| host.ends_with('.')
			|| host.ends_with(".localhost")
			|| host == "localhost"
		{
			return None;
		}
		Some(Self {
			host,
			port: url.port_or_known_default()?,
		})
	}

	/// The origin of an `https` URL, for matching a request against the declared list.
	pub fn of(url: &url::Url) -> Option<Self> {
		if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
			return None;
		}
		let host = match url.host()? {
			url::Host::Domain(host) => host.to_ascii_lowercase(),
			_ => return None,
		};
		Some(Self {
			host,
			port: url.port_or_known_default()?,
		})
	}
}

impl std::fmt::Display for Origin {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		if self.port == 443 {
			write!(f, "https://{}", self.host)
		} else {
			write!(f, "https://{}:{}", self.host, self.port)
		}
	}
}

/// Secrets one manifest may declare.
pub const MAX_SECRETS: usize = 8;
/// Placeholder a request uses for a secret: `{{secret.<id>}}`.
pub const PLACEHOLDER_PREFIX: &str = "{{secret.";

/// A value the user enters for the plugin, sent only to `origins`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
	/// Referenced as `{{secret.<id>}}` in request headers, URLs and bodies.
	pub id: String,
	/// Shown to the user where they enter the value, such as "DeepL API key".
	pub label: String,
	/// Origins that may receive it; each must also be in the manifest's `network` list.
	pub origins: Vec<String>,
}

fn valid_secret_id(id: &str) -> bool {
	(1..=32).contains(&id.len())
		&& id
			.bytes()
			.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

pub(crate) fn validate_secrets(
	secrets: &[SecretSpec],
	network: &std::collections::BTreeSet<Origin>,
) -> Result<(), crate::Error> {
	if secrets.len() > MAX_SECRETS {
		return Err(crate::Error::Limit);
	}
	let mut ids = std::collections::BTreeSet::new();
	for secret in secrets {
		let origins: Option<Vec<Origin>> =
			secret.origins.iter().map(|o| Origin::parse(o)).collect();
		if !valid_secret_id(&secret.id)
			|| !ids.insert(&secret.id)
			|| secret.label.trim().is_empty()
			|| secret.label.len() > 64
			|| secret.label.chars().any(char::is_control)
			|| origins.is_none_or(|origins| {
				origins.is_empty() || origins.iter().any(|origin| !network.contains(origin))
			}) {
			return Err(crate::Error::Invalid);
		}
	}
	Ok(())
}

/// The secret IDs `text` references as `{{secret.<id>}}`, or `None` when a placeholder is
/// malformed.
pub fn placeholders(text: &str) -> Option<Vec<&str>> {
	let mut found = Vec::new();
	let mut rest = text;
	while let Some(start) = rest.find(PLACEHOLDER_PREFIX) {
		let after = &rest[start + PLACEHOLDER_PREFIX.len()..];
		let end = after.find("}}")?;
		let id = &after[..end];
		if !valid_secret_id(id) {
			return None;
		}
		found.push(id);
		rest = &after[end + 2..];
	}
	Some(found)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_exact_named_https_origins_are_accepted() {
		assert_eq!(
			Origin::parse("https://API.Example.org/"),
			Some(Origin {
				host: "api.example.org".into(),
				port: 443
			})
		);
		assert_eq!(
			Origin::parse("https://api.example.org:8443").unwrap().port,
			8443
		);
		for rejected in [
			"http://api.example.org",
			"https://user@api.example.org",
			"https://api.example.org/v1",
			"https://api.example.org/?q=1",
			"https://127.0.0.1",
			"https://[::1]",
			"https://intranet",
			"https://localhost",
			"https://app.localhost",
			"https://*.example.org",
			"wss://api.example.org",
		] {
			assert_eq!(Origin::parse(rejected), None, "{rejected}");
		}
	}

	#[test]
	fn secrets_bind_to_declared_origins_and_placeholders_parse_strictly() {
		let network: std::collections::BTreeSet<Origin> =
			[Origin::parse("https://api.example.org").unwrap()].into();
		let secret = |id: &str, origins: &[&str]| SecretSpec {
			id: id.into(),
			label: "API key".into(),
			origins: origins.iter().map(|o| o.to_string()).collect(),
		};
		validate_secrets(&[secret("api_key", &["https://api.example.org"])], &network).unwrap();
		for invalid in [
			vec![secret("api_key", &["https://other.example.org"])],
			vec![secret("api_key", &[])],
			vec![secret("API-Key", &["https://api.example.org"])],
			vec![
				secret("api_key", &["https://api.example.org"]),
				secret("api_key", &["https://api.example.org"]),
			],
		] {
			assert!(validate_secrets(&invalid, &network).is_err(), "{invalid:?}");
		}
		assert_eq!(
			placeholders("Bearer {{secret.api_key}} and {{secret.other}}"),
			Some(vec!["api_key", "other"])
		);
		assert_eq!(placeholders("no secrets"), Some(vec![]));
		assert_eq!(placeholders("{{secret.api_key"), None);
		assert_eq!(placeholders("{{secret.Bad-Id}}"), None);
	}
}
