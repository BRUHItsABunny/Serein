//! Attachment archiving: `media_archive` plugins queue Discord attachments, and one worker
//! thread saves them into the plugin's private folder after the invocation. Only signed
//! original CDN attachment URLs are fetched, with the same admission rules, size limit, no
//! redirects and no credentials as the client's explicit downloads. Each file must fit
//! the plugin's data limit before it starts, and is written under a temporary name and
//! renamed only once complete.
use extensions::files::{MAX_ARCHIVE_FILE_BYTES, MAX_QUEUED_FETCHES};
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{
	Arc, Condvar, Mutex,
	atomic::{AtomicBool, Ordering},
};

/// Outcomes kept per plugin for the `fetches` request.
const MAX_RESULTS: usize = 256;
/// Fetches waiting across every plugin.
const MAX_QUEUED_TOTAL: usize = 1024;

/// The signed original of a Discord attachment URL, or `None` when it is anything else.
/// A `media.discordapp.net` link names the same object and is rewritten to the CDN.
pub fn cdn_url(raw: &str) -> Option<url::Url> {
	let mut url = url::Url::parse(raw).ok()?;
	if url.host_str() == Some("media.discordapp.net") {
		url.set_host(Some("cdn.discordapp.com")).ok()?;
	}
	let path: Vec<_> = url.path_segments()?.collect();
	let (channel, id, filename) = match path.as_slice() {
		["attachments", channel, id, filename] => (*channel, *id, *filename),
		["attachments", channel, message, id, filename] if message.parse::<u64>().is_ok() => {
			(*channel, *id, *filename)
		}
		_ => return None,
	};
	(url.scheme() == "https"
		&& url.host_str() == Some("cdn.discordapp.com")
		&& url.port_or_known_default() == Some(443)
		&& url.username().is_empty()
		&& url.password().is_none()
		&& url.fragment().is_none()
		&& channel.parse::<u64>().is_ok_and(|id| id > 0)
		&& id.parse::<u64>().is_ok_and(|id| id > 0)
		&& !filename.is_empty()
		&& !filename.contains('\\')
		&& !["%2f", "%5c"]
			.iter()
			.any(|escape| filename.to_ascii_lowercase().contains(escape))
		&& url.query_pairs().all(|(name, value)| match name.as_ref() {
			"ex" | "is" | "hm" => true,
			"backend" => matches!(value.as_ref(), "b2" | "b3"),
			_ => false,
		}))
	.then_some(url)
}

/// One queued fetch.
pub struct Fetch {
	pub url: url::Url,
	pub size: u64,
	/// Where the finished file goes, inside the private folder.
	pub target: PathBuf,
	/// The plugin's path for it, reported back.
	pub path: String,
	pub private: PathBuf,
	pub quota: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchState {
	Queued,
	Done,
	Failed(String),
}

/// Downloads one fetch to `partial`; `cancelled` turns true when the plugin's fetches are
/// dropped.
pub type Downloader = Arc<dyn Fn(&Fetch, &Path, &AtomicBool) -> Result<(), String> + Send + Sync>;

struct Queued {
	fetch: Fetch,
	key: PathBuf,
	cancelled: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
	queue: VecDeque<Queued>,
	/// By the plugin's data directory, which names its account and ID.
	results: BTreeMap<PathBuf, VecDeque<(String, FetchState)>>,
	cancels: BTreeMap<PathBuf, Arc<AtomicBool>>,
	running: bool,
}

pub struct Archive {
	inner: Mutex<Inner>,
	ready: Condvar,
	download: Downloader,
}

impl Default for Archive {
	fn default() -> Self {
		Self::new(Arc::new(download))
	}
}

impl Archive {
	pub fn new(download: Downloader) -> Self {
		Self {
			inner: Mutex::default(),
			ready: Condvar::new(),
			download,
		}
	}
	/// Queues a fetch for the plugin whose data directory is `key`.
	pub fn queue(self: &Arc<Self>, key: &Path, fetch: Fetch) -> Result<(), String> {
		let mut inner = self.inner.lock().map_err(|_| "Archive unavailable")?;
		let waiting = inner
			.queue
			.iter()
			.filter(|queued| queued.key == key)
			.count();
		if waiting >= MAX_QUEUED_FETCHES || inner.queue.len() >= MAX_QUEUED_TOTAL {
			return Err("Too many attachments are waiting; try again later".into());
		}
		if inner
			.queue
			.iter()
			.any(|queued| queued.fetch.target == fetch.target)
		{
			return Err("That path is already being fetched".into());
		}
		let cancelled = Arc::clone(
			inner
				.cancels
				.entry(key.to_path_buf())
				.or_insert_with(|| Arc::new(AtomicBool::new(false))),
		);
		record(&mut inner, key, &fetch.path, FetchState::Queued);
		inner.queue.push_back(Queued {
			fetch,
			key: key.to_path_buf(),
			cancelled,
		});
		if !inner.running {
			inner.running = true;
			let archive = Arc::clone(self);
			if std::thread::Builder::new()
				.name("serein-attachment-archive".into())
				.spawn(move || archive.work())
				.is_err()
			{
				inner.running = false;
			}
		}
		self.ready.notify_one();
		Ok(())
	}
	/// The plugin's recent outcomes, oldest first.
	pub fn results(&self, key: &Path) -> Vec<(String, FetchState)> {
		self.inner
			.lock()
			.ok()
			.and_then(|inner| {
				inner
					.results
					.get(key)
					.map(|list| list.iter().cloned().collect())
			})
			.unwrap_or_default()
	}
	/// Drops the waiting fetches and stops the running one of every plugin whose data
	/// directory is `key` or under it, such as when it is disabled or its account logs out.
	pub fn forget(&self, key: &Path) {
		let Ok(mut inner) = self.inner.lock() else {
			return;
		};
		inner.queue.retain(|queued| !queued.key.starts_with(key));
		let keys: Vec<_> = inner
			.cancels
			.keys()
			.filter(|known| known.starts_with(key))
			.cloned()
			.collect();
		for known in keys {
			if let Some(cancelled) = inner.cancels.remove(&known) {
				cancelled.store(true, Ordering::Release);
			}
			inner.results.remove(&known);
		}
	}
	fn work(self: Arc<Self>) {
		loop {
			let queued = {
				let Ok(mut inner) = self.inner.lock() else {
					return;
				};
				loop {
					if let Some(queued) = inner.queue.pop_front() {
						break queued;
					}
					let Ok((next, timeout)) = self
						.ready
						.wait_timeout(inner, std::time::Duration::from_secs(30))
					else {
						return;
					};
					inner = next;
					// An idle worker ends; the next fetch starts another.
					if timeout.timed_out() && inner.queue.is_empty() {
						inner.running = false;
						return;
					}
				}
			};
			let outcome = self.run(&queued);
			if queued.cancelled.load(Ordering::Acquire) {
				continue;
			}
			if let Ok(mut inner) = self.inner.lock() {
				let state = match outcome {
					Ok(()) => FetchState::Done,
					Err(error) => FetchState::Failed(error),
				};
				record(&mut inner, &queued.key, &queued.fetch.path, state);
			}
		}
	}
	fn run(&self, queued: &Queued) -> Result<(), String> {
		let fetch = &queued.fetch;
		if std::fs::symlink_metadata(&fetch.target).is_ok() {
			return Err("Something already has that name".into());
		}
		let used = crate::files::folder_bytes(&fetch.private)
			.ok_or("The private folder holds too many files")?;
		if used.saturating_add(fetch.size) > fetch.quota {
			return Err("The plugin's data limit is reached".into());
		}
		let parent = fetch.target.parent().ok_or("Invalid path")?;
		std::fs::create_dir_all(parent).map_err(|_| "Cannot create the folder")?;
		let mut partial = fetch.target.clone().into_os_string();
		partial.push(".part");
		let partial = PathBuf::from(partial);
		let result = (self.download)(fetch, &partial, &queued.cancelled).and_then(|()| {
			if queued.cancelled.load(Ordering::Acquire) {
				return Err("Cancelled".into());
			}
			let written = std::fs::metadata(&partial).map_or(0, |metadata| metadata.len());
			if written != fetch.size {
				return Err("The attachment's size changed".into());
			}
			std::fs::rename(&partial, &fetch.target).map_err(|_| "Cannot save the file".into())
		});
		if result.is_err() {
			let _ = std::fs::remove_file(&partial);
		}
		result
	}
}

fn record(inner: &mut Inner, key: &Path, path: &str, state: FetchState) {
	let list = inner.results.entry(key.to_path_buf()).or_default();
	list.retain(|(known, _)| known != path);
	if list.len() >= MAX_RESULTS {
		list.pop_front();
	}
	list.push_back((path.to_owned(), state));
}

/// Downloads `fetch` from Discord's CDN into `partial`, never more than its size.
fn download(fetch: &Fetch, partial: &Path, cancelled: &AtomicBool) -> Result<(), String> {
	use std::io::Write;
	if fetch.size == 0 || fetch.size > MAX_ARCHIVE_FILE_BYTES {
		return Err("Attachments must be nonempty and at most 100 MiB".into());
	}
	tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|_| "Attachment downloads are unavailable")?
		.block_on(async {
			let client = wreq::Client::builder()
				.no_proxy()
				.redirect(wreq::redirect::Policy::none())
				.connect_timeout(std::time::Duration::from_secs(15))
				.read_timeout(std::time::Duration::from_secs(30))
				.timeout(std::time::Duration::from_secs(300))
				.build()
				.map_err(|_| "Attachment downloads are unavailable")?;
			let response = client_core::fingerprint::discord_request(
				client.get(fetch.url.as_str()),
				fetch.url.as_str(),
			)
			.header(wreq::header::ACCEPT_ENCODING, "identity")
			.send()
			.await
			.map_err(|_| "Attachment download failed")?;
			if response.status() != wreq::StatusCode::OK {
				return Err("The attachment is unavailable; its link may have expired".into());
			}
			if response
				.content_length()
				.is_some_and(|length| length != fetch.size)
			{
				return Err("The attachment's size changed".into());
			}
			let mut file = std::fs::OpenOptions::new()
				.create_new(true)
				.write(true)
				.open(partial)
				.map_err(|_| "Cannot create the file")?;
			let mut received = 0u64;
			let mut body = std::pin::pin!(response.bytes_stream());
			while let Some(chunk) = futures_util::StreamExt::next(&mut body)
				.await
				.transpose()
				.map_err(|_| "Attachment download interrupted")?
			{
				if cancelled.load(Ordering::Acquire) {
					return Err("Cancelled".into());
				}
				received += chunk.len() as u64;
				if received > fetch.size {
					return Err("The attachment's size changed".into());
				}
				file.write_all(&chunk)
					.map_err(|_| "Cannot write the file")?;
			}
			Ok(())
		})
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Only signed original attachment links on Discord's CDN are admitted, as for the
	/// client's own downloads.
	#[test]
	fn only_signed_cdn_attachment_urls_are_fetched() {
		let good = "https://cdn.discordapp.com/attachments/1/2/photo.png?ex=1&is=2&hm=3";
		assert!(cdn_url(good).is_some());
		assert_eq!(
			cdn_url("https://media.discordapp.net/attachments/1/9/2/a.mp4?ex=1&is=2&hm=3")
				.unwrap()
				.host_str(),
			Some("cdn.discordapp.com"),
			"proxy links fetch the original"
		);
		for bad in [
			"http://cdn.discordapp.com/attachments/1/2/a.png",
			"https://cdn.discordapp.com:8443/attachments/1/2/a.png",
			"https://user:pass@cdn.discordapp.com/attachments/1/2/a.png",
			"https://cdn.discordapp.com.evil.example/attachments/1/2/a.png",
			"https://example.org/attachments/1/2/a.png",
			"https://cdn.discordapp.com/avatars/1/2.png",
			"https://cdn.discordapp.com/attachments/x/2/a.png",
			"https://cdn.discordapp.com/attachments/1/2/a%2F..%2Fb.png",
			"https://cdn.discordapp.com/attachments/1/2/a.png?width=100",
			"https://cdn.discordapp.com/attachments/1/2/a.png?backend=b9",
			"https://cdn.discordapp.com/attachments/1/2/a.png#frag",
		] {
			assert!(cdn_url(bad).is_none(), "{bad}");
		}
	}

	fn temp() -> PathBuf {
		let mut nonce = [0u8; 8];
		getrandom::fill(&mut nonce).unwrap();
		let path =
			std::env::temp_dir().join(format!("serein-archive-{:016x}", u64::from_le_bytes(nonce)));
		std::fs::create_dir_all(&path).unwrap();
		path
	}

	/// Fetches save through a temporary name, refuse a size mismatch, an existing file and
	/// the data limit, report each outcome, stay bounded per plugin, and stop when the
	/// plugin's fetches are forgotten.
	#[test]
	fn fetches_save_whole_files_within_the_limit_and_report_outcomes() {
		let data = temp();
		let private = data.join("files");
		let (gate_send, gate) = std::sync::mpsc::channel::<()>();
		let gate = Mutex::new(gate);
		let archive = Arc::new(Archive::new(Arc::new(move |fetch, partial, _| {
			if fetch.path == "slow.bin" {
				let _ = gate.lock().unwrap().recv();
			}
			let bytes = if fetch.path == "short.bin" {
				3
			} else {
				fetch.size as usize
			};
			std::fs::write(partial, vec![7; bytes]).map_err(|_| "write".to_string())
		})));
		let fetch = |path: &str, size| Fetch {
			url: cdn_url("https://cdn.discordapp.com/attachments/1/2/a.bin").unwrap(),
			size,
			target: private.join(path),
			path: path.into(),
			private: private.clone(),
			quota: 10,
		};
		archive.queue(&data, fetch("a/one.bin", 4)).unwrap();
		archive.queue(&data, fetch("short.bin", 4)).unwrap();
		archive.queue(&data, fetch("big.bin", 7)).unwrap();
		archive.queue(&data, fetch("slow.bin", 1)).unwrap();
		assert!(
			archive.queue(&data, fetch("slow.bin", 1)).is_err(),
			"one fetch per path"
		);
		let wait = |path: &str| loop {
			let results = archive.results(&data);
			if let Some((_, state)) = results.iter().find(|(known, _)| known == path)
				&& *state != FetchState::Queued
			{
				return state.clone();
			}
			std::thread::yield_now();
		};
		assert_eq!(wait("a/one.bin"), FetchState::Done);
		assert_eq!(
			std::fs::read(private.join("a/one.bin")).unwrap(),
			vec![7; 4]
		);
		assert!(matches!(wait("short.bin"), FetchState::Failed(_)));
		assert!(!private.join("short.bin").exists() && !private.join("short.bin.part").exists());
		assert!(
			matches!(wait("big.bin"), FetchState::Failed(error) if error.contains("limit")),
			"4 + 7 bytes exceed 10"
		);
		// While one fetch runs, at most 256 more may wait.
		let refused = (0..=MAX_QUEUED_FETCHES)
			.filter(|index| {
				archive
					.queue(&data, fetch(&format!("wait-{index}.bin"), 0))
					.is_err()
			})
			.count();
		assert!(refused >= 1);
		// Forgetting the plugin drops its fetches and outcomes.
		archive.forget(&data);
		assert!(archive.results(&data).is_empty());
		gate_send.send(()).unwrap();
		let _ = std::fs::remove_dir_all(data);
	}
}
