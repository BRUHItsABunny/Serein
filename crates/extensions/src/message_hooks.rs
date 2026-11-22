//! Message hooks: a `send` action that may rewrite the user's draft before it is sent, and an
//! `annotate` action that adds a note under messages, such as a translation. Both run only in
//! channels where the user switched the plugin on.
use crate::{Error, MAX_EVENT_CONTENT_BYTES, Manifest, app};
use serde::{Deserialize, Serialize};

/// Most messages in one `annotate` batch.
pub const MAX_ANNOTATE_MESSAGES: usize = 25;
/// Largest total message text in one `annotate` batch, so it fits the input buffer.
pub const MAX_ANNOTATE_BYTES: usize = 128 * 1024;
/// Largest note under one message, in UTF-8 bytes.
pub const MAX_ANNOTATION_BYTES: usize = 4 * 1024;

/// Where a `send` action's draft is going. The draft itself is in `composer`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendTarget {
	pub channel_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub guild_id: Option<String>,
}

impl SendTarget {
	pub fn validate(&self) -> Result<(), Error> {
		app::entity_id(&self.channel_id)?;
		if let Some(guild) = &self.guild_id {
			app::entity_id(guild)?;
		}
		Ok(())
	}
}

/// Messages of one channel for an `annotate` action, oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotateBatch {
	pub channel_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub guild_id: Option<String>,
	pub messages: Vec<AnnotateMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotateMessage {
	pub message_id: String,
	pub author_id: String,
	pub content: String,
	/// The message was edited; a note for its earlier text is replaced.
	#[serde(default, skip_serializing_if = "crate::interface_ui::is_false")]
	pub edited: bool,
}

impl AnnotateBatch {
	pub fn validate(&self) -> Result<(), Error> {
		SendTarget {
			channel_id: self.channel_id.clone(),
			guild_id: self.guild_id.clone(),
		}
		.validate()?;
		if self.messages.is_empty() || self.messages.len() > MAX_ANNOTATE_MESSAGES {
			return Err(Error::Limit);
		}
		let mut total = 0;
		for message in &self.messages {
			app::entity_id(&message.message_id)?;
			app::entity_id(&message.author_id)?;
			total += message.content.len();
			if message.content.len() > MAX_EVENT_CONTENT_BYTES || total > MAX_ANNOTATE_BYTES {
				return Err(Error::Limit);
			}
		}
		Ok(())
	}
}

/// A note shown under one message; empty text removes the plugin's note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annotation {
	pub message_id: String,
	pub text: String,
}

/// Checks returned notes: each names a message the plugin was given, at most once, and fits.
pub(crate) fn validate_annotations(
	annotations: &[Annotation],
	allowed: &[&str],
) -> Result<(), Error> {
	if annotations.len() > MAX_ANNOTATE_MESSAGES {
		return Err(Error::Limit);
	}
	let mut seen = std::collections::BTreeSet::new();
	for annotation in annotations {
		if !allowed.contains(&annotation.message_id.as_str())
			|| !seen.insert(&annotation.message_id)
		{
			return Err(Error::Invalid);
		}
		if annotation.text.len() > MAX_ANNOTATION_BYTES {
			return Err(Error::Limit);
		}
	}
	Ok(())
}

/// A hook action needs its capability, and its capability needs the action.
pub(crate) fn validate(manifest: &Manifest) -> Result<(), Error> {
	use crate::{Capability, Surface};
	for (surface, capability) in [
		(Surface::Send, Capability::SendHook),
		(Surface::Annotate, Capability::MessageAnnotations),
	] {
		let has_action = manifest
			.actions
			.iter()
			.any(|action| action.surface == surface);
		if surface == Surface::Send && has_action != manifest.capabilities.contains(&capability) {
			return Err(Error::Capability);
		}
		// `message_annotations` also lets `message` actions annotate the chosen message.
		if has_action && !manifest.capabilities.contains(&capability) {
			return Err(Error::Capability);
		}
	}
	Ok(())
}
