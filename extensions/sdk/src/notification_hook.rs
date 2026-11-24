//! Notification hook (preview): see each desktop alert before it is shown and reword or hide
//! it. The host shows the original alert when the plugin fails or answers too slowly.
use crate::{Invocation, Output};
use serde::{Deserialize, Serialize};

/// Largest alert title, in UTF-8 bytes.
pub const MAX_NOTIFICATION_TITLE_BYTES: usize = 256;
/// Largest alert body, in UTF-8 bytes.
pub const MAX_NOTIFICATION_BODY_BYTES: usize = 512;

/// Input of a [`crate::Surface::Notification`] action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationInvocation {
	#[serde(flatten)]
	pub invocation: Invocation,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub notification: Option<Notification>,
}

/// Where the host learned about the message.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationSource {
	#[default]
	Gateway,
	/// A mobile push message; the same message from the gateway may later replace it.
	Push,
	/// A source this SDK version does not know.
	#[serde(other)]
	Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
	pub source: NotificationSource,
	pub message_id: String,
	pub channel_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub guild_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub author_id: Option<String>,
	/// From a direct or group conversation.
	#[serde(default)]
	pub direct: bool,
	pub title: String,
	pub body: String,
	/// This alert was already shown; a reworded one replaces it.
	#[serde(default)]
	pub upgrade: bool,
}

/// How the alert is shown. Omitted fields keep it as it was. Title and body take no control
/// characters, line breaks included.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationDecision {
	/// Do not show it. Sounds and unread badges are unaffected.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub hide: bool,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub title: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub body: Option<String>,
}

/// Output of a `notification` action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationOutput {
	#[serde(flatten)]
	pub output: Output,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub notification: Option<NotificationDecision>,
}
