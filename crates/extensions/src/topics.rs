//! Topics plugins broadcast to each other. A manifest declares the versioned topics it
//! `broadcasts` and `subscribes` to; the user approves each subscriber, provider and topic
//! before anything flows. Payloads are JSON the host never interprets.
use crate::{Error, Manifest, Surface};

/// Topics one manifest may list in each of `broadcasts` and `subscribes`.
pub const MAX_TOPICS: usize = 8;
/// Largest serialized payload of one broadcast.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
/// Largest `broadcast` request: the payload plus its topic and framing.
pub const MAX_REQUEST_BYTES: u32 = 17 * 1024;
/// Most messages one `topic_read` returns.
pub const MAX_READ: u32 = 64;

/// A topic name: dot-separated lowercase segments, a slash and a version, such as
/// `serein.captions/1`. At least two segments, at most 64 characters.
pub fn valid_name(name: &str) -> bool {
	let Some((path, version)) = name.split_once('/') else {
		return false;
	};
	name.len() <= 64
		&& !version.is_empty()
		&& version.len() <= 4
		&& version.bytes().all(|byte| byte.is_ascii_digit())
		&& !version.starts_with('0')
		&& path.split('.').count() >= 2
		&& path.split('.').all(|segment| {
			!segment.is_empty()
				&& segment.bytes().all(|byte| {
					byte.is_ascii_lowercase()
						|| byte.is_ascii_digit()
						|| matches!(byte, b'-' | b'_')
				})
		})
}

pub(crate) fn validate(manifest: &Manifest) -> Result<(), Error> {
	for list in [&manifest.broadcasts, &manifest.subscribes] {
		if list.len() > MAX_TOPICS {
			return Err(Error::Limit);
		}
		let mut seen = std::collections::BTreeSet::new();
		if list
			.iter()
			.any(|topic| !valid_name(topic) || !seen.insert(topic))
		{
			return Err(Error::Invalid);
		}
	}
	let receives = manifest
		.actions
		.iter()
		.any(|action| action.surface == Surface::Topic);
	// Subscribing means having somewhere to deliver, and only subscribers get deliveries.
	if receives == manifest.subscribes.is_empty() {
		return Err(Error::Capability);
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn topic_names_are_versioned_lowercase_paths() {
		for valid in ["serein.captions/1", "example.my-topic_2/12"] {
			assert!(valid_name(valid), "{valid}");
		}
		for invalid in [
			"captions/1",
			"serein.captions",
			"serein.captions/0",
			"serein.captions/01",
			"Serein.captions/1",
			"serein..captions/1",
			"serein.captions/v1",
			"serein.captions/1/2",
		] {
			assert!(!valid_name(invalid), "{invalid}");
		}
	}
}
