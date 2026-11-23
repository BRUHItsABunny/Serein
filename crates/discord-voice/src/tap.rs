//! Media taps: copies of decrypted remote media for plugins, taken only while switched on.
//! Each copy is made on the network task right after DAVE decryption, never in an audio
//! callback, and lands in one bounded queue a consumer drains off the UI thread. Packets
//! that do not fit are counted per stream instead.
use crate::video_receive::Lost;
use opus2::{Channels, Decoder};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{
	Mutex, OnceLock,
	atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

/// Packets waiting for the consumer.
pub const MAX_PACKETS: usize = 4096;
/// Payload bytes waiting for the consumer.
pub const MAX_BYTES: usize = 16 * 1024 * 1024;
/// Samples per second of converted audio.
pub const AUDIO_RATE: u32 = 16_000;

/// Which of a participant's streams a packet belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
	/// Microphone audio.
	Voice,
	Camera,
	/// Screen share video.
	Screen,
	/// Screen share audio.
	ScreenAudio,
}

impl Source {
	pub fn name(self) -> &'static str {
		match self {
			Self::Voice => "voice",
			Self::Camera => "camera",
			Self::Screen => "screen",
			Self::ScreenAudio => "screen_audio",
		}
	}
	pub fn audio(self) -> bool {
		matches!(self, Self::Voice | Self::ScreenAudio)
	}
}

/// One decrypted packet: an Opus frame or a whole video access unit.
#[derive(Clone, Debug)]
pub struct Packet {
	pub source: Source,
	pub user: u64,
	/// `opus`, `h264`, `h265` or `av1`.
	pub codec: &'static str,
	pub received: Instant,
	pub rtp_timestamp: u32,
	/// RTP sequence number of audio packets.
	pub sequence: Option<u16>,
	pub keyframe: bool,
	/// The user's own media, copied as sent; `user` is then the user's own ID.
	pub own: bool,
	pub data: Vec<u8>,
}

#[derive(Default)]
struct Queue {
	packets: VecDeque<Packet>,
	bytes: usize,
	lost: BTreeMap<(Source, u64), u32>,
}

#[derive(Default)]
struct Tap {
	enabled: AtomicBool,
	/// Also copy the user's own media, for consumers granted it.
	own: AtomicBool,
	queue: Mutex<Queue>,
	camera_keyframes: Lost,
	screen_keyframes: Lost,
}

fn tap() -> &'static Tap {
	static TAP: OnceLock<Tap> = OnceLock::new();
	TAP.get_or_init(Tap::default)
}

/// Switches copying on or off, and copying the user's own media with it; off discards what
/// is queued.
pub fn set_enabled(enabled: bool, own: bool) {
	let tap = tap();
	tap.own.store(enabled && own, Ordering::Release);
	tap.enabled.store(enabled, Ordering::Release);
	if !enabled && let Ok(mut queue) = tap.queue.lock() {
		*queue = Queue::default();
	}
}

pub(crate) fn enabled() -> bool {
	tap().enabled.load(Ordering::Relaxed)
}

/// Copies the user's own media as it is sent, before encryption, while that is wanted.
pub(crate) fn push_own(
	source: Source,
	user: u64,
	codec: &'static str,
	rtp_timestamp: u32,
	sequence: Option<u16>,
	keyframe: bool,
	data: &[u8],
) {
	if !tap().own.load(Ordering::Relaxed) {
		return;
	}
	push(Packet {
		source,
		user,
		codec,
		received: Instant::now(),
		rtp_timestamp,
		sequence,
		keyframe,
		own: true,
		data: data.to_vec(),
	});
}

/// Queues a copy, or counts it lost when the queue is full.
pub(crate) fn push(packet: Packet) {
	let Ok(mut queue) = tap().queue.lock() else {
		return;
	};
	if queue.packets.len() >= MAX_PACKETS || queue.bytes + packet.data.len() > MAX_BYTES {
		*queue.lost.entry((packet.source, packet.user)).or_default() += 1;
		return;
	}
	queue.bytes += packet.data.len();
	queue.packets.push_back(packet);
}

/// Everything queued, and how many packets of each stream were lost since the last drain.
pub fn drain() -> (Vec<Packet>, Vec<(Source, u64, u32)>) {
	let Ok(mut queue) = tap().queue.lock() else {
		return Default::default();
	};
	let queue = std::mem::take(&mut *queue);
	(
		queue.packets.into(),
		queue
			.lost
			.into_iter()
			.map(|((source, user), lost)| (source, user, lost))
			.collect(),
	)
}

/// Asks `user`'s camera or screen share for a keyframe, through the same path the client
/// uses to recover lost pictures, so it is rate-limited the same way.
pub fn request_keyframe(source: Source, user: u64) {
	let list = match source {
		Source::Camera => &tap().camera_keyframes,
		Source::Screen => &tap().screen_keyframes,
		Source::Voice | Source::ScreenAudio => return,
	};
	if let Ok(mut users) = list.lock()
		&& !users.contains(&user)
		&& users.len() < crate::crypto::MAX_PARTICIPANTS
	{
		users.push(user);
	}
}

pub(crate) fn keyframe_requests(source: Source) -> &'static Lost {
	match source {
		Source::Screen => &tap().screen_keyframes,
		_ => &tap().camera_keyframes,
	}
}

/// Decodes tapped Opus into 16 kHz mono 16-bit samples, one decoder per stream.
#[derive(Default)]
pub struct AudioConverter {
	decoders: HashMap<(Source, u64), Decoder>,
}

impl AudioConverter {
	/// The packet's samples, or `None` for video or an undecodable packet.
	pub fn convert(&mut self, packet: &Packet) -> Option<Vec<i16>> {
		if !packet.source.audio() {
			return None;
		}
		let key = (packet.source, packet.user);
		if !self.decoders.contains_key(&key) {
			if self.decoders.len() >= crate::crypto::MAX_PARTICIPANTS * 2 {
				return None;
			}
			self.decoders
				.insert(key, Decoder::new(AUDIO_RATE, Channels::Mono).ok()?);
		}
		let decoder = self.decoders.get_mut(&key)?;
		// 120 ms, the longest Opus packet.
		let mut pcm = vec![0_i16; AUDIO_RATE as usize * 120 / 1000];
		let samples = decoder.decode(&packet.data, &mut pcm, false).ok()?;
		pcm.truncate(samples);
		Some(pcm)
	}
	pub fn clear(&mut self) {
		self.decoders.clear();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn packet(user: u64, bytes: usize) -> Packet {
		Packet {
			source: Source::Voice,
			user,
			codec: "opus",
			received: Instant::now(),
			rtp_timestamp: 0,
			sequence: Some(1),
			keyframe: false,
			own: false,
			data: vec![0xF8; bytes],
		}
	}

	/// Nothing is copied while off; the queue is bounded by packets and bytes and counts
	/// what it could not keep per stream; switching off discards it; and tapped Opus decodes
	/// to 20 ms of 16 kHz audio.
	#[test]
	fn taps_copy_only_while_on_and_count_what_did_not_fit() {
		set_enabled(false, false);
		assert!(!enabled());
		set_enabled(true, false);
		for _ in 0..MAX_PACKETS + 3 {
			push(packet(1, 1));
		}
		push(packet(2, MAX_BYTES));
		let (packets, lost) = drain();
		assert_eq!(packets.len(), MAX_PACKETS);
		assert_eq!(lost, vec![(Source::Voice, 1, 3), (Source::Voice, 2, 1)]);
		assert!(drain().0.is_empty(), "drained once");
		push(packet(1, 1));
		set_enabled(false, false);
		assert!(drain().0.is_empty());

		// A 20 ms Opus frame decodes to 320 samples at 16 kHz.
		let mut encoder =
			opus2::Encoder::new(48_000, Channels::Mono, opus2::Application::Voip).unwrap();
		let mut opus = vec![0; 1275];
		let length = encoder.encode(&[0_i16; 960], &mut opus).unwrap();
		opus.truncate(length);
		let mut converter = AudioConverter::default();
		let mut voice = packet(1, 0);
		voice.data = opus;
		assert_eq!(converter.convert(&voice).map(|pcm| pcm.len()), Some(320));
		voice.source = Source::Camera;
		assert!(converter.convert(&voice).is_none());
	}
}
