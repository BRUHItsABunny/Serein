//! Media worker: a plugin instance that stays loaded during a call and receives what happens
//! in it in batches, such as participants joining and, with media taps, audio and video.
//! Binary payloads are not in the JSON; the plugin copies them with `serein.media_read`.
use crate::{Error, app};
use serde::{Deserialize, Serialize};

/// Most items in one batch.
pub const MAX_MEDIA_ITEMS: usize = 256;
/// Largest binary payload of one batch.
pub const MAX_MEDIA_DATA_BYTES: u32 = 4 * 1024 * 1024;
/// Largest stream name or codec, in bytes.
pub const MAX_MEDIA_NAME_BYTES: usize = 64;

/// Items in time order since the previous batch. `dropped` counts batches the host
/// discarded because the plugin fell behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MediaBatch {
	pub seq: u64,
	#[serde(default, skip_serializing_if = "is_zero")]
	pub dropped: u32,
	/// Bytes of binary payload behind this batch, read with `media_read`.
	#[serde(default, skip_serializing_if = "is_zero")]
	pub data_bytes: u32,
	pub items: Vec<MediaItem>,
}

fn is_zero(value: &u32) -> bool {
	*value == 0
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MediaItem {
	/// Something that happened in the call, at `time_us` on the host's media clock.
	Event { time_us: u64, event: CallEvent },
	/// A piece of one stream's media in the batch's binary payload.
	Chunk {
		stream: String,
		codec: String,
		time_us: u64,
		offset: u32,
		length: u32,
	},
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CallEvent {
	/// The worker's call began; the first item of the first batch.
	Started {
		channel_id: String,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		guild_id: Option<String>,
	},
	Joined {
		user_id: String,
	},
	Left {
		user_id: String,
	},
	/// The call ended; the worker stops after this batch.
	Ended,
}

fn name(text: &str) -> Result<(), Error> {
	if text.is_empty()
		|| text.len() > MAX_MEDIA_NAME_BYTES
		|| !text
			.bytes()
			.all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'/' | b':'))
	{
		return Err(Error::Invalid);
	}
	Ok(())
}

impl MediaBatch {
	pub fn validate(&self) -> Result<(), Error> {
		if self.items.len() > MAX_MEDIA_ITEMS || self.data_bytes > MAX_MEDIA_DATA_BYTES {
			return Err(Error::Limit);
		}
		for item in &self.items {
			match item {
				MediaItem::Event { event, .. } => match event {
					CallEvent::Started {
						channel_id,
						guild_id,
					} => {
						app::entity_id(channel_id)?;
						if let Some(guild) = guild_id {
							app::entity_id(guild)?;
						}
					}
					CallEvent::Joined { user_id } | CallEvent::Left { user_id } => {
						app::entity_id(user_id)?
					}
					CallEvent::Ended => {}
				},
				MediaItem::Chunk {
					stream,
					codec,
					offset,
					length,
					..
				} => {
					name(stream)?;
					name(codec)?;
					if offset
						.checked_add(*length)
						.is_none_or(|end| end > self.data_bytes)
					{
						return Err(Error::Invalid);
					}
				}
			}
		}
		Ok(())
	}
}
