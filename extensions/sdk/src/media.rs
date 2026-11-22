//! Media worker (preview): one instance stays loaded during a call and runs your `media`
//! action once per batch, so statics and heap state carry over between batches. Copy a
//! chunk's bytes with [`crate::host::media::read`].
use crate::Invocation;
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
	Chunk {
		stream: String,
		codec: String,
		time_us: u64,
		offset: u32,
		length: u32,
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
				{"type":"video_keyframe"}]}}"#,
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
	}
}
