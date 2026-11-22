//! Call captions (preview): a few short lines shown above the call controls during a call.
use crate::Output;
use serde::{Deserialize, Serialize};

/// Most caption lines shown for one plugin.
pub const MAX_CAPTIONS: usize = 4;
/// Largest caption line, in UTF-8 bytes; no control characters.
pub const MAX_CAPTION_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Caption {
	/// The speaking user's ID; the host shows their name.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub speaker_id: Option<String>,
	pub text: String,
	/// The line will not change again.
	#[serde(rename = "final", default, skip_serializing_if = "std::ops::Not::not")]
	pub done: bool,
}

/// Output carrying your caption lines, which replace the ones shown; an empty list clears
/// them. Lines fade out 10 seconds after your last update.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptionOutput {
	#[serde(flatten)]
	pub output: Output,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub captions: Option<Vec<Caption>>,
}
