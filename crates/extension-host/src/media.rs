//! Media workers: one thread per plugin and call, holding a [`MediaInstance`] that stays
//! loaded and runs the plugin's `media` action once per batch. The queue in front of it is
//! bounded by batches and bytes; when the plugin falls behind, the oldest batches are
//! dropped and the next batch reports how many. A failed call ends the worker.
use extensions::{Caption, Invocation, MediaBatch, MediaInstance, MediaItem};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, mpsc};

/// Batches waiting for the plugin.
const MAX_QUEUED: usize = 32;
/// Payload bytes waiting for the plugin.
const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;

/// What a worker reports back.
#[derive(Debug, PartialEq, Eq)]
pub enum MediaEvent {
	/// Caption lines a batch returned.
	Captions(Vec<Caption>),
	/// The worker stopped after an error; a new one may be started.
	Failed(String),
}

#[derive(Default)]
struct Queue {
	batches: VecDeque<(Vec<MediaItem>, Vec<u8>)>,
	bytes: usize,
	dropped: u32,
	/// No more batches come; finish the queued ones and exit.
	closing: bool,
}

#[derive(Default)]
struct Shared {
	queue: Mutex<Queue>,
	ready: Condvar,
}

/// Loads the plugin for a worker: its instance and its `media` action.
pub type Starter = Box<dyn FnOnce() -> Result<(MediaInstance, String), String> + Send>;

pub struct MediaWorker {
	shared: Arc<Shared>,
	events: mpsc::Receiver<MediaEvent>,
	thread: Option<std::thread::JoinHandle<()>>,
}

impl MediaWorker {
	/// Starts a worker thread; `wake` runs after each event so the front end polls.
	pub fn spawn(start: Starter, wake: Arc<dyn Fn() + Send + Sync>) -> Self {
		let shared = Arc::new(Shared::default());
		let (send, events) = mpsc::sync_channel(16);
		let worker = Arc::clone(&shared);
		let thread = std::thread::Builder::new()
			.name("serein-media-plugin".into())
			.spawn(move || {
				let report = |event| {
					let _ = send.try_send(event);
					wake();
				};
				let (mut instance, action) = match start() {
					Ok(started) => started,
					Err(error) => return report(MediaEvent::Failed(error)),
				};
				let mut seq = 0;
				while let Some((items, data, dropped)) = next(&worker) {
					seq += 1;
					let invocation = Invocation {
						action: action.clone(),
						media: Some(Box::new(MediaBatch {
							seq,
							dropped,
							data_bytes: data.len() as u32,
							items,
						})),
						..Default::default()
					};
					match instance.invoke(&invocation, data) {
						Ok(output) => {
							if let Some(captions) = output.captions {
								report(MediaEvent::Captions(captions));
							}
						}
						Err(error) => return report(MediaEvent::Failed(error.to_string())),
					}
				}
			})
			.ok();
		Self {
			shared,
			events,
			thread,
		}
	}
	/// Queues a batch of at most [`extensions::media::MAX_MEDIA_ITEMS`] items whose chunks
	/// point into `data`. Over the bound, the oldest queued batches are dropped.
	pub fn push(&self, items: Vec<MediaItem>, data: Vec<u8>) {
		let Ok(mut queue) = self.shared.queue.lock() else {
			return;
		};
		if queue.closing || items.is_empty() {
			return;
		}
		queue.bytes += data.len();
		queue.batches.push_back((items, data));
		while queue.batches.len() > MAX_QUEUED || queue.bytes > MAX_QUEUED_BYTES {
			let Some((_, data)) = queue.batches.pop_front() else {
				break;
			};
			queue.bytes -= data.len();
			queue.dropped = queue.dropped.saturating_add(1);
		}
		self.shared.ready.notify_one();
	}
	/// Lets the worker finish what is queued, then end. Nothing is waited for.
	pub fn close(&self) {
		if let Ok(mut queue) = self.shared.queue.lock() {
			queue.closing = true;
		}
		self.shared.ready.notify_one();
	}
	pub fn try_event(&self) -> Option<MediaEvent> {
		self.events.try_recv().ok()
	}
	/// Waits up to five seconds for the next event.
	#[cfg(test)]
	pub(crate) fn wait_event(&self) -> Option<MediaEvent> {
		self.events
			.recv_timeout(std::time::Duration::from_secs(5))
			.ok()
	}
	/// Whether the thread is still running.
	pub fn running(&self) -> bool {
		self.thread
			.as_ref()
			.is_some_and(|thread| !thread.is_finished())
	}
}

impl Drop for MediaWorker {
	fn drop(&mut self) {
		self.close();
	}
}

/// Waits for the next batch, with how many were dropped before it; `None` once closed and
/// drained.
fn next(shared: &Shared) -> Option<(Vec<MediaItem>, Vec<u8>, u32)> {
	let mut queue = shared.queue.lock().ok()?;
	loop {
		if let Some((items, data)) = queue.batches.pop_front() {
			queue.bytes -= data.len();
			let dropped = std::mem::take(&mut queue.dropped);
			return Some((items, data, dropped));
		}
		if queue.closing {
			return None;
		}
		queue = shared.ready.wait(queue).ok()?;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A plugin that falls behind loses the oldest batches, by count and by bytes, and the
	/// next batch it gets says how many; closing drains what is queued.
	#[test]
	fn queued_batches_are_bounded_and_report_what_was_dropped() {
		let (_, events) = mpsc::sync_channel(1);
		let worker = MediaWorker {
			shared: Arc::new(Shared::default()),
			events,
			thread: None,
		};
		let item = |seq: u64| MediaItem::Event {
			time_us: seq,
			event: extensions::CallEvent::Ended,
		};
		for seq in 0..MAX_QUEUED as u64 + 8 {
			worker.push(vec![item(seq)], Vec::new());
		}
		let (items, _, dropped) = next(&worker.shared).unwrap();
		assert_eq!((items, dropped), (vec![item(8)], 8));
		let (_, _, dropped) = next(&worker.shared).unwrap();
		assert_eq!(dropped, 0, "reported once");
		// Bytes bound the queue too.
		worker.push(vec![item(100)], vec![0; MAX_QUEUED_BYTES]);
		worker.push(vec![item(101)], vec![0; 1]);
		worker.close();
		let mut last = None;
		while let Some(batch) = next(&worker.shared) {
			last = Some(batch);
		}
		let (items, data, _) = last.unwrap();
		assert_eq!((items, data.len()), (vec![item(101)], 1));
		worker.push(vec![item(102)], Vec::new());
		assert!(
			next(&worker.shared).is_none(),
			"nothing is queued after closing"
		);
	}
}
