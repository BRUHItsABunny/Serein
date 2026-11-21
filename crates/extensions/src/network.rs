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
}
