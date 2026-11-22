//! Notification hook: a `notification` action sees each desktop alert before it is shown and
//! may reword or hide it. The host shows the original when the plugin fails or is too slow.
use crate::{Error, app};
use serde::{Deserialize, Serialize};

/// Largest alert title, in UTF-8 bytes.
pub const MAX_NOTIFICATION_TITLE_BYTES: usize = 256;
/// Largest alert body, in UTF-8 bytes.
pub const MAX_NOTIFICATION_BODY_BYTES: usize = 512;

/// Where the host learned about the message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSource {
	/// The live connection to Discord.
	#[default]
	Gateway,
	/// A mobile push message, which may carry less detail; the same message from the gateway
	/// later replaces it.
	Push,
}

/// An alert the host is about to show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationInput {
	pub source: NotificationSource,
	pub message_id: String,
	pub channel_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub guild_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub author_id: Option<String>,
	/// The message is from a direct or group conversation.
	#[serde(default, skip_serializing_if = "crate::interface_ui::is_false")]
	pub direct: bool,
	pub title: String,
	pub body: String,
	/// The alert was already shown, and a reworded one replaces it.
	#[serde(default, skip_serializing_if = "crate::interface_ui::is_false")]
	pub upgrade: bool,
}

impl NotificationInput {
	pub fn validate(&self) -> Result<(), Error> {
		app::entity_id(&self.message_id)?;
		app::entity_id(&self.channel_id)?;
		for id in self.guild_id.iter().chain(&self.author_id) {
			app::entity_id(id)?;
		}
		text(&self.title, MAX_NOTIFICATION_TITLE_BYTES)?;
		text(&self.body, MAX_NOTIFICATION_BODY_BYTES)
	}
}

/// A `notification` action's answer. Omitted fields keep the alert as it was.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationDecision {
	/// Do not show this alert. Sounds and unread badges are unaffected.
	#[serde(default, skip_serializing_if = "crate::interface_ui::is_false")]
	pub hide: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub body: Option<String>,
}

impl NotificationDecision {
	pub fn validate(&self) -> Result<(), Error> {
		if self.hide && (self.title.is_some() || self.body.is_some()) {
			return Err(Error::Invalid);
		}
		if let Some(title) = &self.title {
			text(title, MAX_NOTIFICATION_TITLE_BYTES)?;
			if title.trim().is_empty() {
				return Err(Error::Invalid);
			}
		}
		if let Some(body) = &self.body {
			text(body, MAX_NOTIFICATION_BODY_BYTES)?;
		}
		Ok(())
	}
}

fn text(text: &str, bytes: usize) -> Result<(), Error> {
	if text.len() > bytes {
		return Err(Error::Limit);
	}
	if text.chars().any(char::is_control) {
		return Err(Error::Invalid);
	}
	Ok(())
}
