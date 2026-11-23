//! Moves tapped call media to media plugins on its own thread, never the UI thread: it
//! drains the voice tap every 20 ms, decodes audio once for the plugins that want samples,
//! and queues bounded batches on each plugin's media worker.
use discord_voice::tap::{self, AudioConverter, Packet};
use extension_host::media::MediaSender;
use extensions::{
	MediaItem,
	media::{MAX_MEDIA_DATA_BYTES, MAX_MEDIA_ITEMS},
};
use std::sync::{
	Arc, Mutex,
	atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const INTERVAL: Duration = Duration::from_millis(20);

/// A media worker and what it may receive.
#[derive(Clone)]
pub struct Target {
	pub plugin: String,
	pub sender: MediaSender,
	/// Decrypted packets (`media_packets`).
	pub packets: bool,
	/// 16 kHz samples (`media_audio`).
	pub audio: bool,
	/// The user's own media too (`media_own`).
	pub own: bool,
	/// The worker's media clock starts here.
	pub started: Instant,
}

#[derive(Default)]
pub struct Pump {
	targets: Arc<Mutex<Vec<Target>>>,
	/// What the targets were last set to, so unchanged ones are not handed over again.
	key: Vec<(String, bool, bool, bool)>,
	running: Option<Arc<AtomicBool>>,
}

impl Pump {
	/// Replaces the workers media goes to; the tap is on only while one of them wants it.
	pub fn set_targets(&mut self, targets: Vec<Target>) {
		let key: Vec<_> = targets
			.iter()
			.map(|target| {
				(
					target.plugin.clone(),
					target.packets,
					target.audio,
					target.own,
				)
			})
			.collect();
		if key == self.key {
			return;
		}
		self.key = key;
		let wanted = targets.iter().any(|target| target.packets || target.audio);
		let own = targets.iter().any(|target| target.packets && target.own);
		if let Ok(mut current) = self.targets.lock() {
			*current = targets;
		}
		tap::set_enabled(wanted, own);
		match (&self.running, wanted) {
			(None, true) => {
				let running = Arc::new(AtomicBool::new(true));
				let (flag, targets) = (Arc::clone(&running), Arc::clone(&self.targets));
				if std::thread::Builder::new()
					.name("serein-media-tap".into())
					.spawn(move || run(&targets, &flag))
					.is_ok()
				{
					self.running = Some(running);
				}
			}
			(Some(running), false) => {
				running.store(false, Ordering::Release);
				self.running = None;
			}
			_ => {}
		}
	}
}

impl Drop for Pump {
	fn drop(&mut self) {
		self.set_targets(Vec::new());
	}
}

fn run(targets: &Mutex<Vec<Target>>, running: &AtomicBool) {
	let mut converter = AudioConverter::default();
	while running.load(Ordering::Acquire) {
		std::thread::sleep(INTERVAL);
		let (packets, lost) = tap::drain();
		if packets.is_empty() && lost.is_empty() {
			continue;
		}
		let Ok(targets) = targets.lock().map(|targets| targets.clone()) else {
			return;
		};
		let samples: Vec<Option<Vec<u8>>> = if targets.iter().any(|target| target.audio) {
			packets
				.iter()
				.map(|packet| {
					converter
						.convert(packet)
						.map(|pcm| pcm.iter().flat_map(|sample| sample.to_le_bytes()).collect())
				})
				.collect()
		} else {
			Vec::new()
		};
		for target in &targets {
			for (items, data) in batches(target, &packets, &samples, &lost, Instant::now()) {
				target.sender.push(items, data);
			}
		}
	}
	converter.clear();
}

fn stream(packet: &Packet) -> String {
	format!("{}:{}", packet.source.name(), packet.user)
}

/// Splits what one target may receive into batches of at most [`MAX_MEDIA_ITEMS`] items and
/// [`MAX_MEDIA_DATA_BYTES`] of data. A payload too large for any batch becomes a gap.
fn batches(
	target: &Target,
	packets: &[Packet],
	samples: &[Option<Vec<u8>>],
	lost: &[(tap::Source, u64, u32)],
	now: Instant,
) -> Vec<(Vec<MediaItem>, Vec<u8>)> {
	let clock = |at: Instant| at.saturating_duration_since(target.started).as_micros() as u64;
	let mut out = Vec::new();
	let mut items = Vec::new();
	let mut data = Vec::new();
	let mut add =
		|item: Option<(MediaItem, &[u8])>, items: &mut Vec<MediaItem>, data: &mut Vec<u8>| {
			let Some((mut item, bytes)) = item else {
				return;
			};
			if items.len() >= MAX_MEDIA_ITEMS
				|| data.len() + bytes.len() > MAX_MEDIA_DATA_BYTES as usize
			{
				out.push((std::mem::take(items), std::mem::take(data)));
			}
			if let MediaItem::Chunk { offset, .. } = &mut item {
				*offset = data.len() as u32;
			}
			data.extend_from_slice(bytes);
			items.push(item);
		};
	let gap = |stream: String, lost: u32| MediaItem::Gap {
		stream,
		time_us: clock(now),
		lost,
	};
	for (index, packet) in packets.iter().enumerate() {
		if packet.own && !target.own {
			continue;
		}
		let chunk = |codec: &str, bytes: &[u8]| MediaItem::Chunk {
			stream: stream(packet),
			codec: codec.into(),
			time_us: clock(packet.received),
			offset: 0,
			length: bytes.len() as u32,
			rtp_timestamp: Some(packet.rtp_timestamp),
			sequence: packet.sequence,
			keyframe: packet.keyframe,
		};
		if target.packets {
			let item = if packet.data.len() > MAX_MEDIA_DATA_BYTES as usize {
				(gap(stream(packet), 1), &[][..])
			} else {
				(chunk(packet.codec, &packet.data), &packet.data[..])
			};
			add(Some(item), &mut items, &mut data);
		}
		if target.audio
			&& let Some(Some(pcm)) = samples.get(index)
		{
			add(
				Some((chunk("pcm_s16le", pcm), &pcm[..])),
				&mut items,
				&mut data,
			);
		}
	}
	for (source, user, count) in lost {
		if target.packets || target.audio && source.audio() {
			add(
				Some((gap(format!("{}:{user}", source.name()), *count), &[][..])),
				&mut items,
				&mut data,
			);
		}
	}
	if !items.is_empty() {
		out.push((items, data));
	}
	out
}

#[cfg(test)]
mod tests {
	use super::*;

	fn packet(source: tap::Source, user: u64, bytes: usize, at: Instant) -> Packet {
		Packet {
			source,
			user,
			codec: if source.audio() { "opus" } else { "h264" },
			received: at,
			rtp_timestamp: 960,
			sequence: source.audio().then_some(7),
			keyframe: !source.audio(),
			own: false,
			data: vec![1; bytes],
		}
	}

	/// Each target gets only what its grants allow, chunks point at their own bytes,
	/// batches split before an item or byte limit, an oversized picture becomes a gap, and
	/// losses reach targets of that kind.
	#[test]
	fn batches_follow_grants_point_at_their_bytes_and_split_at_the_limits() {
		let (worker, _) = (
			extension_host::media::MediaWorker::spawn(
				Box::new(|| Err("unused".into())),
				extension_host::media::Backlog::LIVE,
				Arc::new(|| {}),
			),
			(),
		);
		let started = Instant::now();
		let target = |packets, audio| Target {
			plugin: "p".into(),
			sender: worker.sender(),
			packets,
			audio,
			own: false,
			started,
		};
		let at = started + Duration::from_millis(5);
		let packets = vec![
			packet(tap::Source::Voice, 5, 3, at),
			packet(tap::Source::Camera, 5, 4, at),
			packet(
				tap::Source::Screen,
				6,
				MAX_MEDIA_DATA_BYTES as usize + 1,
				at,
			),
		];
		let samples = vec![Some(vec![9, 9]), None, None];
		let lost = vec![(tap::Source::Camera, 5, 2), (tap::Source::Voice, 5, 1)];

		let audio = batches(&target(false, true), &packets, &samples, &lost, at);
		let [(items, data)] = &audio[..] else {
			panic!("{audio:?}")
		};
		assert_eq!(data, &vec![9, 9]);
		assert!(
			matches!(
				&items[..],
				[
					MediaItem::Chunk { stream, codec, offset: 0, length: 2, time_us: 5000, .. },
					MediaItem::Gap { lost: 1, .. },
				] if stream == "voice:5" && codec == "pcm_s16le"
			),
			"audio only, with the voice gap but not the camera's"
		);

		let all = batches(&target(true, false), &packets, &samples, &lost, at);
		let [(items, data)] = &all[..] else {
			panic!("{all:?}")
		};
		assert_eq!(data.len(), 7);
		assert!(matches!(
			items[1],
			MediaItem::Chunk {
				offset: 3,
				length: 4,
				keyframe: true,
				..
			}
		));
		assert!(
			matches!(&items[2], MediaItem::Gap { stream, lost: 1, .. } if stream == "screen:6")
		);
		assert_eq!(items.len(), 5);

		// The user's own media reaches only targets granted it.
		let mine = vec![Packet {
			own: true,
			..packet(tap::Source::Voice, 1, 2, at)
		}];
		assert!(batches(&target(true, false), &mine, &[], &[], at).is_empty());
		let granted = Target {
			own: true,
			..target(true, false)
		};
		assert!(matches!(
			&batches(&granted, &mine, &[], &[], at)[..],
			[(items, _)] if matches!(&items[..], [MediaItem::Chunk { stream, .. }] if stream == "voice:1")
		));

		let many: Vec<_> = (0..MAX_MEDIA_ITEMS + 1)
			.map(|_| packet(tap::Source::Voice, 5, 1, at))
			.collect();
		let split = batches(&target(true, false), &many, &[], &[], at);
		assert_eq!(
			split
				.iter()
				.map(|(items, _)| items.len())
				.collect::<Vec<_>>(),
			[MAX_MEDIA_ITEMS, 1]
		);
		assert!(
			matches!(split[1].0[0], MediaItem::Chunk { offset: 0, .. }),
			"offsets restart per batch"
		);
	}
}
