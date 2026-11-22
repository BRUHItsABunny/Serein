//! Message hooks (preview): rewrite drafts before they are sent and add notes under messages.
//! Both run only in channels where the user switched the plugin on.
use crate::{Invocation, Output};
use serde::{Deserialize, Serialize};

/// Most messages in one [`AnnotateBatch`], and most notes one result may return.
pub const MAX_ANNOTATE_MESSAGES: usize = 25;
/// Largest note under one message, in UTF-8 bytes.
pub const MAX_ANNOTATION_BYTES: usize = 4 * 1024;

/// Input of a [`crate::Surface::Send`] action. The draft is in `invocation.composer`; return
/// the text to send in `Output::replacement`, or `None` to send the draft unchanged. Whether
/// the user reviews the rewrite first is their choice per channel.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendInvocation {
	#[serde(flatten)]
	pub invocation: Invocation,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub send_target: Option<SendTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendTarget {
	pub channel_id: String,
	/// Absent in direct and group messages.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub guild_id: Option<String>,
}

/// Input of a [`crate::Surface::Annotate`] action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotateInvocation {
	#[serde(flatten)]
	pub invocation: Invocation,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub annotate: Option<AnnotateBatch>,
}

/// Up to [`MAX_ANNOTATE_MESSAGES`] messages of one channel, oldest first: new and edited
/// messages, and the latest ones when the user opens the channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotateBatch {
	pub channel_id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub guild_id: Option<String>,
	pub messages: Vec<AnnotateMessage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotateMessage {
	pub message_id: String,
	pub author_id: String,
	pub content: String,
	/// The message was edited since it was last given to this plugin.
	#[serde(default)]
	pub edited: bool,
}

/// A note shown under a message; empty `text` removes this plugin's note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
	pub message_id: String,
	pub text: String,
}

/// Output of an `annotate` action, or of a `message` action annotating the chosen message.
/// Each note must name a message the action was given.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnotationOutput {
	#[serde(flatten)]
	pub output: Output,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub annotations: Vec<Annotation>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hook_inputs_decode_and_annotations_flatten_into_the_output() {
		let send: SendInvocation = serde_json::from_str(
			r#"{"action":"send","composer":"hallo","send_target":{"channel_id":"20"}}"#,
		)
		.unwrap();
		assert_eq!(send.invocation.composer.as_deref(), Some("hallo"));
		assert_eq!(send.send_target.unwrap().channel_id, "20");
		let batch: AnnotateInvocation = serde_json::from_str(
			r#"{"action":"notes","annotate":{"channel_id":"20","messages":[
				{"message_id":"1","author_id":"2","content":"hola"}]}}"#,
		)
		.unwrap();
		assert!(!batch.annotate.unwrap().messages[0].edited);
		let output = serde_json::to_value(AnnotationOutput {
			annotations: vec![Annotation {
				message_id: "1".into(),
				text: "hello".into(),
			}],
			..Default::default()
		})
		.unwrap();
		assert_eq!(output["annotations"][0]["text"], "hello");
		assert!(
			output.get("panel").is_some(),
			"Output fields stay flattened"
		);
	}
}
