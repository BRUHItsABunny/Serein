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

/// Text shown in place of a message the plugin was given, such as its translation, with a
/// toggle back to the message itself; empty text removes this plugin's alternate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alternate {
	pub message_id: String,
	/// What the text is, such as "Translation", shown beside the toggle.
	pub label: String,
	pub text: String,
}

/// Longest alternate label, in UTF-8 bytes.
pub const MAX_ALTERNATE_LABEL_BYTES: usize = 64;
/// Longest follow-up message from a send hook, in UTF-8 bytes.
pub const MAX_FOLLOW_UP_BYTES: usize = 16 * 1024;

/// Earlier versions of a message the plugin was given, oldest first, shown behind its
/// "edited" label; an empty list removes this plugin's history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessageHistory {
	pub message_id: String,
	pub revisions: Vec<crate::PageRevision>,
}

/// Deleted messages the plugin kept, shown as deleted rows under the message they followed;
/// an empty list removes this plugin's rows there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletedMessages {
	pub after_message_id: String,
	pub messages: Vec<DeletedMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletedMessage {
	pub message_id: String,
	pub author: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub time_ms: Option<i64>,
	pub text: String,
	/// When it was deleted.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub deleted_ms: Option<i64>,
	/// Its earlier versions, oldest first, at most 8.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub revisions: Vec<crate::pages::PageRevision>,
}

/// Most deleted rows one result may add under all its messages.
pub const MAX_DELETED_ROWS: usize = 25;

/// Checks returned histories and deleted rows against the messages the plugin was given.
pub(crate) fn validate_history(
	histories: &[MessageHistory],
	deleted: &[DeletedMessages],
	allowed: &[&str],
) -> Result<(), Error> {
	if histories.len() > MAX_ANNOTATE_MESSAGES || deleted.len() > MAX_ANNOTATE_MESSAGES {
		return Err(Error::Limit);
	}
	let anchors = histories
		.iter()
		.map(|history| history.message_id.as_str())
		.chain(deleted.iter().map(|rows| rows.after_message_id.as_str()));
	let mut seen = std::collections::BTreeSet::new();
	for (index, id) in anchors.enumerate() {
		// Each message once among histories, and once among deleted rows.
		let list = usize::from(index >= histories.len());
		if !allowed.contains(&id) || !seen.insert((list, id)) {
			return Err(Error::Invalid);
		}
	}
	let text = |text: &str| {
		if text.len() > crate::pages::MAX_PAGE_ROW_TEXT_BYTES {
			Err(Error::Limit)
		} else if text.chars().any(|c| c.is_control() && c != '\n') {
			Err(Error::Invalid)
		} else {
			Ok(())
		}
	};
	for history in histories {
		if history.revisions.len() > crate::pages::MAX_PAGE_REVISIONS {
			return Err(Error::Limit);
		}
		for revision in &history.revisions {
			text(&revision.text)?;
		}
	}
	let mut rows = 0;
	for group in deleted {
		rows += group.messages.len();
		if rows > MAX_DELETED_ROWS {
			return Err(Error::Limit);
		}
		for message in &group.messages {
			app::entity_id(&message.message_id)?;
			// A deleted row is not one of the loaded messages it sits between.
			if allowed.contains(&message.message_id.as_str()) {
				return Err(Error::Invalid);
			}
			if message.author.len() > crate::pages::MAX_PAGE_AUTHOR_BYTES
				|| message.author.trim().is_empty()
				|| message.author.chars().any(char::is_control)
			{
				return Err(Error::Invalid);
			}
			text(&message.text)?;
			if message.revisions.len() > crate::pages::MAX_PAGE_REVISIONS {
				return Err(Error::Limit);
			}
			for revision in &message.revisions {
				text(&revision.text)?;
			}
		}
	}
	Ok(())
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

/// Checks returned alternates: each names a given message once, with a short plain label.
pub(crate) fn validate_alternates(alternates: &[Alternate], allowed: &[&str]) -> Result<(), Error> {
	if alternates.len() > MAX_ANNOTATE_MESSAGES {
		return Err(Error::Limit);
	}
	let mut seen = std::collections::BTreeSet::new();
	for alternate in alternates {
		if !allowed.contains(&alternate.message_id.as_str()) || !seen.insert(&alternate.message_id)
		{
			return Err(Error::Invalid);
		}
		if alternate.text.len() > MAX_ANNOTATION_BYTES
			|| alternate.label.len() > MAX_ALTERNATE_LABEL_BYTES
		{
			return Err(Error::Limit);
		}
		if alternate.text.chars().any(|c| c.is_control() && c != '\n')
			|| alternate.label.chars().any(char::is_control)
			|| !alternate.text.is_empty() && alternate.label.trim().is_empty()
		{
			return Err(Error::Invalid);
		}
	}
	Ok(())
}

/// A follow-up is a non-blank message within its byte limit.
pub(crate) fn validate_follow_up(text: &str) -> Result<(), Error> {
	if text.len() > MAX_FOLLOW_UP_BYTES {
		Err(Error::Limit)
	} else if text.trim().is_empty() {
		Err(Error::Invalid)
	} else {
		Ok(())
	}
}

/// A hook action needs its capability, and its capability needs the action.
/// The notification hook is checked here too.
pub(crate) fn validate(manifest: &Manifest) -> Result<(), Error> {
	use crate::{Capability, Surface};
	for (surface, capability) in [
		(Surface::Send, Capability::SendHook),
		(Surface::Annotate, Capability::MessageAnnotations),
		(Surface::Notification, Capability::Notifications),
		(Surface::Media, Capability::MediaWorker),
	] {
		let has_action = manifest
			.actions
			.iter()
			.any(|action| action.surface == surface);
		if surface != Surface::Annotate && has_action != manifest.capabilities.contains(&capability)
		{
			return Err(Error::Capability);
		}
		// `message_annotations` also lets `message` actions annotate the chosen message.
		if has_action && !manifest.capabilities.contains(&capability) {
			return Err(Error::Capability);
		}
	}
	Ok(())
}
