//! Call captions: up to a few short lines a plugin shows above the call controls, such as a
//! live transcription or translation. They are drawn by the host and never sent to Discord.
use crate::{Error, app};
use serde::{Deserialize, Serialize};

/// Most caption lines one plugin shows at a time.
pub const MAX_CAPTIONS: usize = 4;
/// Largest caption line, in UTF-8 bytes.
pub const MAX_CAPTION_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caption {
	/// Who is speaking; the host shows their name in the call.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub speaker_id: Option<String>,
	pub text: String,
	/// The line will not change again; unfinished lines are drawn dimmer.
	#[serde(
		rename = "final",
		default,
		skip_serializing_if = "crate::interface_ui::is_false"
	)]
	pub done: bool,
}

/// Checks a plugin's caption lines; an empty list clears them.
pub fn validate(captions: &[Caption]) -> Result<(), Error> {
	if captions.len() > MAX_CAPTIONS {
		return Err(Error::Limit);
	}
	for caption in captions {
		if let Some(speaker) = &caption.speaker_id {
			app::entity_id(speaker)?;
		}
		if caption.text.len() > MAX_CAPTION_BYTES {
			return Err(Error::Limit);
		}
		if caption.text.trim().is_empty() || caption.text.chars().any(char::is_control) {
			return Err(Error::Invalid);
		}
	}
	Ok(())
}
