//! Media worker (preview): one instance stays loaded during a call and runs your `media`
//! action once per batch, so statics and heap state carry over between batches. Copy a
//! chunk's bytes with [`crate::host::media::read`].
use crate::{Caption, Invocation, Output};
use serde::{Deserialize, Serialize};

/// Input of a [`crate::Surface::Media`] action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaInvocation {
	#[serde(flatten)]
	pub invocation: Invocation,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub media: Option<MediaBatch>,
}

/// Up to 256 items in time order, with at most 4 MiB of binary data behind them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaBatch {
	pub seq: u64,
	/// Batches dropped before this one because you fell behind.
	#[serde(default)]
	pub dropped: u32,
	#[serde(default)]
	pub data_bytes: u32,
	pub items: Vec<MediaItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MediaItem {
	/// Something that happened in the call, in microseconds since the worker started.
	Event { time_us: u64, event: CallEvent },
	/// A piece of one stream's media at `offset..offset + length` of the batch's data.
	/// Streams are `voice:<user>`, `camera:<user>`, `screen:<user>` and
	/// `screen_audio:<user>`. With `media_packets`, `codec` is `opus` (one frame), `h264`,
	/// `h265` or `av1` (one access unit); with `media_audio`, `pcm_s16le` is 16 kHz mono
	/// little-endian samples decoded from one Opus frame.
	Chunk {
		stream: String,
		codec: String,
		time_us: u64,
		offset: u32,
		length: u32,
		/// The sender's RTP timestamp: 48 kHz for audio, 90 kHz for video.
		#[serde(default)]
		rtp_timestamp: Option<u32>,
		#[serde(default)]
		sequence: Option<u16>,
		#[serde(default)]
		keyframe: bool,
	},
	/// Packets of `stream` the host dropped because they did not fit its queue.
	Gap {
		stream: String,
		time_us: u64,
		lost: u32,
	},
	/// What your other actions last set with [`MediaStateOutput::media_state`]: in the
	/// first batch of each worker, and in the next batch whenever it changes.
	State {
		time_us: u64,
		state: serde_json::Value,
	},
	/// An item this SDK version does not know.
	#[serde(other)]
	Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CallEvent {
	Started {
		channel_id: String,
		#[serde(default)]
		guild_id: Option<String>,
	},
	Joined {
		user_id: String,
	},
	Left {
		user_id: String,
	},
	/// The call ended; this is the last batch.
	Ended,
	#[serde(other)]
	Other,
}

/// Output of a `media` action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaOutput {
	#[serde(flatten)]
	pub output: Output,
	/// Requires `call_captions`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub captions: Option<Vec<Caption>>,
	/// Up to 16 `camera:<user>` or `screen:<user>` streams to ask a keyframe for, such as
	/// when a recording starts. Requires `media_packets`.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub keyframes: Vec<String>,
	/// `Some(false)` stops media chunks while you only wait for events, such as someone
	/// joining; `Some(true)` resumes them. Requires `media_packets` or `media_audio`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub media_chunks: Option<bool>,
}

/// Largest [`MediaStateOutput::media_state`], as JSON.
pub const MAX_MEDIA_STATE_BYTES: usize = 16 * 1024;

/// Output of any action but `media`, `send`, `annotate` and `notification` that tells your
/// media worker something, such as its settings or "record this call".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStateOutput {
	#[serde(flatten)]
	pub output: Output,
	/// Replaces the state your media worker is given; it is kept in memory until Serein
	/// closes, so set it again from your `activation` action. Requires `media_worker`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub media_state: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn batches_decode_with_unknown_items_and_events() {
		let input: MediaInvocation = serde_json::from_str(
			r#"{"action":"media","media":{"seq":1,"data_bytes":5,"items":[
				{"type":"event","time_us":0,"event":{"kind":"started","channel_id":"20"}},
				{"type":"event","time_us":1,"event":{"kind":"raised_hand","user_id":"5"}},
				{"type":"chunk","stream":"user:5","codec":"opus","time_us":2,"offset":0,"length":5},
				{"type":"video_keyframe"},
				{"type":"state","time_us":3,"state":{"record":true}}]}}"#,
		)
		.unwrap();
		let items = input.media.unwrap().items;
		assert!(matches!(
			items[1],
			MediaItem::Event {
				event: CallEvent::Other,
				..
			}
		));
		assert!(matches!(items[2], MediaItem::Chunk { length: 5, .. }));
		assert_eq!(items[3], MediaItem::Other);
		assert!(matches!(&items[4], MediaItem::State { state, .. } if state["record"] == true));
	}
}
