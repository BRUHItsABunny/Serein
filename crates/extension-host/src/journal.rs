//! Per-account message journal for plugins granted `message_journal`.
//!
//! The front end turns gateway events into journal records before the reducer applies them
//! ([`capture`]), so an edit or delete can carry the earlier version of a message the state
//! still holds. A [`Recorder`] thread appends the records to `journal.sqlite` in the
//! account's extension directory. It runs only while an enabled plugin consumes the
//! journal; records are never kept for nobody.
//!
//! Each consumer has a cursor. A `journal` action pulls entries after it with
//! `serein.journal_read`, and the host advances the cursor only once the invocation is
//! accepted, after the plugin's own database commits ([`JournalSession`]). Delivery is
//! therefore at least once: a failed or interrupted invocation sees the same entries again.
//!
//! Nothing is lost silently. A full record queue, a failed write or entries pruned past
//! the journal's bounds become `gap` entries, and a new gateway session records
//! `session_started` plus a `missed` entry for each known channel whose latest message moved
//! while nobody was listening.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;

use client_core::{Event, State, auth::AuthState};
use extensions::Reply;
use model::{Id, Message, MessagePatch, Patch};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

use crate::store::Wake;

/// File name inside the account's extension directory.
pub const FILE_NAME: &str = "journal.sqlite";
/// Entries kept for consumers that have not read them yet.
const MAX_ENTRIES: i64 = 100_000;
/// Serialized entry bytes kept for consumers that have not read them yet.
const MAX_BYTES: i64 = 64 * 1024 * 1024;
/// One entry's serialized size; larger messages drop attachment and embed details.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// Records waiting for the writer; more become an `overflow` gap.
const QUEUE: usize = 1024;
/// Reply budget of one `journal_read`, below the host's 256 KiB pending-reply limit.
const MAX_BATCH_BYTES: usize = 192 * 1024;
/// Channels whose latest journaled message is remembered for `missed` entries.
const MAX_CHANNELS: i64 = 10_000;
/// Bulk deletes Discord sends hold at most this many IDs.
const MAX_BULK: usize = 100;

/// One journal record built on the UI thread; the writer adds its sequence and time.
pub enum Record {
	Entry {
		body: String,
		/// `(channel, message)` of a new message, for `missed` detection.
		latest: Option<(u64, u64)>,
	},
	/// Latest message ID per channel after a new gateway session.
	Seen(Vec<(u64, u64)>),
}

fn recording(state: &State) -> bool {
	state.user.as_ref().is_some_and(|user| user.id.0 != 0)
		&& (state.demo || state.auth == AuthState::Authenticated)
}

fn guild(state: &State, channel: Id) -> Option<String> {
	state
		.channel(channel)
		.and_then(|channel| channel.guild)
		.map(|guild| guild.0.to_string())
}

fn millis(nanos: i128) -> i64 {
	(nanos / 1_000_000) as i64
}

/// Creation time encoded in a Discord snowflake.
fn created_ms(id: Id) -> i64 {
	((id.0 >> 22) as i64).saturating_add(1_420_070_400_000)
}

fn attachments(attachments: &[model::Attachment]) -> Value {
	attachments
		.iter()
		.map(|attachment| {
			json!({
				"id": attachment.id.0.to_string(),
				"filename": attachment.filename,
				"content_type": attachment.content_type,
				"size": attachment.size,
				"url": attachment.media.url,
				"proxy_url": attachment.media.proxy_url,
			})
		})
		.collect()
}

/// The archived form of a message: stable, documented fields rather than the host model.
fn message(message: &Message, detailed: bool) -> Value {
	let mut value = json!({
		"id": message.id.0.to_string(),
		"author": {
			"id": message.author.id.0.to_string(),
			"name": message.author.name,
			"nick": message.author_nick,
			"bot": message.author.kind != model::AccountKind::Human,
			"webhook": message.author.webhook,
		},
		"content": message.content,
		"created_at_ms": created_ms(message.id),
		"edited_at_ms": message.edited_at.map(millis),
		"type": message.kind,
		"flags": message.flags,
		"reply_to": message.reply_to.map(|id| id.0.to_string()),
		"forwarded": message.forwarded,
		"embeds": message.embeds.len(),
		"stickers": message.sticker_items.len(),
	});
	if detailed {
		value["attachments"] = attachments(&message.attachments);
	} else {
		value["truncated"] = true.into();
	}
	value
}

/// Serializes `build(true)`, falling back to `build(false)` when an entry is too large.
fn encode(build: impl Fn(bool) -> Value) -> Option<String> {
	let body = build(true).to_string();
	if body.len() <= MAX_ENTRY_BYTES {
		return Some(body);
	}
	let body = build(false).to_string();
	(body.len() <= MAX_ENTRY_BYTES).then_some(body)
}

fn previous(state: &State, channel: Id, id: Id) -> Option<&Message> {
	state
		.timeline
		.get(id)
		.filter(|message| message.channel == channel)
}

fn update(state: &State, patch: &MessagePatch) -> Option<String> {
	if !matches!(patch.content, Patch::Value(_))
		&& !matches!(patch.edited, Patch::Value(_))
		&& !matches!(patch.attachments, Patch::Value(_))
	{
		// Embed unfurls, reactions and similar refreshes are not edits.
		return None;
	}
	let prior = previous(state, patch.channel, patch.id);
	encode(|detailed| {
		let mut value = json!({
			"kind": "update",
			"channel_id": patch.channel.0.to_string(),
			"guild_id": guild(state, patch.channel),
			"message_id": patch.id.0.to_string(),
			"previous": prior.map(|message| self::message(message, detailed)),
		});
		if let Patch::Value(content) = &patch.content {
			value["content"] = content.as_str().into();
		}
		if let Patch::Value(edited) = patch.edited {
			value["edited_at_ms"] = millis(edited).into();
		}
		if let (Patch::Value(list), true) = (&patch.attachments, detailed) {
			value["attachments"] = attachments(list);
		}
		value
	})
}

fn delete(state: &State, channel: Id, id: Id, bulk: bool) -> Option<String> {
	let prior = previous(state, channel, id);
	encode(|detailed| {
		json!({
			"kind": "delete",
			"channel_id": channel.0.to_string(),
			"guild_id": guild(state, channel),
			"message_id": id.0.to_string(),
			"bulk": bulk,
			"previous": prior.map(|message| self::message(message, detailed)),
		})
	})
}

fn entry(body: Option<String>) -> Option<Record> {
	body.map(|body| Record::Entry { body, latest: None })
}

/// Records for `event`, captured before the reducer applies it so the timeline still holds
/// the version an edit or delete replaces. Call only for events of the current generation.
pub fn capture(state: &State, event: &Event) -> Vec<Record> {
	if !recording(state) {
		return Vec::new();
	}
	match event {
		Event::Message(created) if !created.ephemeral && created.id.0 != 0 => {
			let latest = Some((created.channel.0, created.id.0));
			encode(|detailed| {
				json!({
					"kind": "create",
					"channel_id": created.channel.0.to_string(),
					"guild_id": guild(state, created.channel),
					"message": message(created, detailed),
				})
			})
			.map(|body| vec![Record::Entry { body, latest }])
			.unwrap_or_default()
		}
		Event::Patch(patch) if patch.id.0 != 0 => entry(update(state, patch)).into_iter().collect(),
		Event::Delete { channel, id } if id.0 != 0 => entry(delete(state, *channel, *id, false))
			.into_iter()
			.collect(),
		Event::DeleteBulk { channel, ids } => ids
			.iter()
			.take(MAX_BULK)
			.filter(|id| id.0 != 0)
			.filter_map(|id| entry(delete(state, *channel, *id, true)))
			.collect(),
		Event::Ready { .. } | Event::Resync => entry(Some(r#"{"kind":"session_started"}"#.into()))
			.into_iter()
			.collect(),
		Event::ChannelCreated(channel) => entry(Some(
			json!({ "kind": "channel_changed", "channel_id": channel.id.0.to_string() })
				.to_string(),
		))
		.into_iter()
		.collect(),
		Event::ChannelChanged(patch) => entry(Some(
			json!({ "kind": "channel_changed", "channel_id": patch.id.0.to_string() }).to_string(),
		))
		.into_iter()
		.collect(),
		event if event.changes_access() => entry(Some(r#"{"kind":"access_changed"}"#.into()))
			.into_iter()
			.collect(),
		_ => Vec::new(),
	}
}

/// Records that need the reducer's result: after a new gateway session, each channel's
/// latest message, so the writer can flag channels that moved while nobody listened.
pub fn capture_after(state: &State, event: &Event) -> Vec<Record> {
	if !recording(state) || !matches!(event, Event::Ready { .. } | Event::Resync) {
		return Vec::new();
	}
	let seen: Vec<_> = state
		.channels
		.iter()
		.filter_map(|channel| channel.last_message.map(|last| (channel.id.0, last.0)))
		.take(MAX_CHANNELS as usize)
		.collect();
	vec![Record::Seen(seen)]
}

/// Records for a recorder that just opened: the session the account is in, and where
/// each channel stands, so `missed` reports what arrived while nobody listened.
pub fn opened(state: &State) -> Vec<Record> {
	let mut records = capture(state, &Event::Resync);
	records.extend(capture_after(state, &Event::Resync));
	records
}

enum Command {
	Record(Record),
	/// Wakes the writer to apply a changed consumer list.
	Consumers,
}

/// Owns the journal writer thread for one account while it has consumers.
pub struct Recorder {
	sender: Option<mpsc::SyncSender<Command>>,
	dropped: Arc<AtomicU64>,
	appended: Arc<AtomicBool>,
	consumers: Arc<Mutex<Option<Vec<String>>>>,
	thread: Option<JoinHandle<()>>,
}

impl Recorder {
	/// Starts the writer for `path` with the plugins currently consuming the journal.
	/// `wake` runs after each commit that added entries.
	pub fn open(path: PathBuf, consumers: Vec<String>, wake: Wake) -> Self {
		let (sender, receiver) = mpsc::sync_channel(QUEUE);
		let dropped = Arc::new(AtomicU64::new(0));
		let appended = Arc::new(AtomicBool::new(false));
		let pending = Arc::new(Mutex::new(Some(consumers)));
		let thread = {
			let (dropped, appended, pending) = (dropped.clone(), appended.clone(), pending.clone());
			std::thread::Builder::new()
				.name("serein-journal".into())
				.spawn(move || write_loop(&path, &receiver, &dropped, &appended, &pending, &wake))
				.ok()
		};
		let _ = sender.try_send(Command::Consumers);
		Self {
			sender: Some(sender),
			dropped,
			appended,
			consumers: pending,
			thread,
		}
	}

	/// Queues records without blocking; a full queue becomes an `overflow` gap.
	pub fn record(&self, records: Vec<Record>) {
		let Some(sender) = &self.sender else {
			return;
		};
		for record in records {
			if let Err(mpsc::TrySendError::Full(_)) = sender.try_send(Command::Record(record)) {
				self.dropped.fetch_add(1, Ordering::Relaxed);
			}
		}
	}

	/// Replaces the consuming plugins. New ones start after the latest entry; removed ones
	/// lose their cursor, so entries only they had not read are pruned.
	pub fn set_consumers(&self, consumers: Vec<String>) {
		if let Ok(mut pending) = self.consumers.lock() {
			*pending = Some(consumers);
		}
		if let Some(sender) = &self.sender {
			let _ = sender.try_send(Command::Consumers);
		}
	}

	/// Whether entries were committed since the last call.
	pub fn take_appended(&self) -> bool {
		self.appended.swap(false, Ordering::AcqRel)
	}
}

impl Drop for Recorder {
	/// Waits for queued records to be written, so the file is closed before logout cleanup.
	fn drop(&mut self) {
		self.sender = None;
		if let Some(thread) = self.thread.take() {
			let _ = thread.join();
		}
	}
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
	if let Some(parent) = path.parent() {
		let _ = std::fs::create_dir_all(parent);
	}
	let connection = Connection::open(path)?;
	connection.busy_timeout(std::time::Duration::from_secs(5))?;
	connection.execute_batch(
		"PRAGMA journal_mode = WAL;
		 PRAGMA synchronous = NORMAL;
		 CREATE TABLE IF NOT EXISTS entries (
			seq INTEGER PRIMARY KEY AUTOINCREMENT,
			at_ms INTEGER NOT NULL,
			body TEXT NOT NULL
		 );
		 CREATE TABLE IF NOT EXISTS cursors (plugin TEXT PRIMARY KEY, seq INTEGER NOT NULL);
		 CREATE TABLE IF NOT EXISTS channels (channel INTEGER PRIMARY KEY, last INTEGER NOT NULL);",
	)?;
	Ok(connection)
}

fn now_ms() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn write_loop(
	path: &Path,
	receiver: &mpsc::Receiver<Command>,
	dropped: &AtomicU64,
	appended: &AtomicBool,
	consumers: &Mutex<Option<Vec<String>>>,
	wake: &Wake,
) {
	let mut connection = open(path).ok();
	while let Ok(first) = receiver.recv() {
		let mut batch = vec![first];
		while batch.len() < QUEUE {
			match receiver.try_recv() {
				Ok(command) => batch.push(command),
				Err(_) => break,
			}
		}
		if connection.is_none() {
			connection = open(path).ok();
		}
		let records = batch
			.iter()
			.filter(|command| matches!(command, Command::Record(_)))
			.count() as u64;
		let written = connection.as_mut().map(|connection| {
			write(
				connection,
				batch,
				dropped,
				consumers.lock().ok().and_then(|mut c| c.take()),
			)
		});
		match written {
			Some(Ok(true)) => {
				appended.store(true, Ordering::Release);
				wake();
			}
			Some(Ok(false)) => {}
			// The records are gone; the next successful write reports them as a gap.
			_ => {
				dropped.fetch_add(records, Ordering::Relaxed);
			}
		}
	}
}

/// Writes one batch in a transaction; returns whether any entry was added.
fn write(
	connection: &mut Connection,
	batch: Vec<Command>,
	dropped: &AtomicU64,
	consumers: Option<Vec<String>>,
) -> rusqlite::Result<bool> {
	let transaction = connection.transaction()?;
	if let Some(consumers) = consumers {
		sync_consumers(&transaction, &consumers)?;
	}
	let listening: i64 = transaction.query_row("SELECT count(*) FROM cursors", [], |r| r.get(0))?;
	let at = now_ms();
	let mut added = false;
	let mut insert = |body: &str| -> rusqlite::Result<()> {
		added = true;
		transaction.execute(
			"INSERT INTO entries (at_ms, body) VALUES (?1, ?2)",
			params![at, body],
		)?;
		Ok(())
	};
	if listening > 0 {
		let lost = dropped.swap(0, Ordering::Relaxed);
		if lost > 0 {
			insert(&json!({ "kind": "gap", "reason": "overflow", "dropped": lost }).to_string())?;
		}
	}
	for command in batch {
		let Command::Record(record) = command else {
			continue;
		};
		match record {
			Record::Entry { body, latest } => {
				if listening > 0 {
					insert(&body)?;
				}
				if let Some((channel, message)) = latest {
					transaction.execute(
						"INSERT INTO channels (channel, last) VALUES (?1, ?2)
						 ON CONFLICT (channel) DO UPDATE SET last = max(last, excluded.last)",
						params![channel as i64, message as i64],
					)?;
				}
			}
			Record::Seen(seen) => {
				for (channel, latest) in seen {
					let last: Option<i64> = transaction
						.query_row(
							"SELECT last FROM channels WHERE channel = ?1",
							[channel as i64],
							|row| row.get(0),
						)
						.optional()?;
					if listening > 0
						&& let Some(last) = last
						&& last < latest as i64
					{
						insert(
							&json!({
								"kind": "missed",
								"channel_id": channel.to_string(),
								"after_message_id": last.to_string(),
								"latest_message_id": latest.to_string(),
							})
							.to_string(),
						)?;
					}
					transaction.execute(
						"INSERT INTO channels (channel, last) VALUES (?1, ?2)
						 ON CONFLICT (channel) DO UPDATE SET last = max(last, excluded.last)",
						params![channel as i64, latest as i64],
					)?;
				}
			}
		}
	}
	prune(&transaction)?;
	transaction.commit()?;
	Ok(added)
}

fn sync_consumers(connection: &Connection, consumers: &[String]) -> rusqlite::Result<()> {
	let existing: Vec<String> = connection
		.prepare("SELECT plugin FROM cursors")?
		.query_map([], |row| row.get(0))?
		.collect::<rusqlite::Result<_>>()?;
	for plugin in existing.iter().filter(|plugin| !consumers.contains(plugin)) {
		connection.execute("DELETE FROM cursors WHERE plugin = ?1", [plugin])?;
	}
	for plugin in consumers {
		// New consumers start after the newest sequence ever issued.
		connection.execute(
			"INSERT OR IGNORE INTO cursors (plugin, seq) VALUES (?1,
				coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'entries'), 0))",
			[plugin],
		)?;
	}
	Ok(())
}

/// Drops entries every consumer has read, then the oldest beyond the journal's bounds;
/// consumers behind those see a `pruned` gap.
fn prune(connection: &Connection) -> rusqlite::Result<()> {
	connection.execute(
		"DELETE FROM entries WHERE seq <= coalesce((SELECT min(seq) FROM cursors),
			(SELECT max(seq) FROM entries))",
		[],
	)?;
	loop {
		let (count, bytes): (i64, i64) = connection.query_row(
			"SELECT count(*), coalesce(sum(length(body)), 0) FROM entries",
			[],
			|row| Ok((row.get(0)?, row.get(1)?)),
		)?;
		if count <= MAX_ENTRIES && bytes <= MAX_BYTES {
			break;
		}
		connection.execute(
			"DELETE FROM entries WHERE seq IN (SELECT seq FROM entries ORDER BY seq LIMIT ?1)",
			[(count - MAX_ENTRIES).max(1_000)],
		)?;
	}
	connection.execute(
		"DELETE FROM channels WHERE channel NOT IN
			(SELECT channel FROM channels ORDER BY last DESC LIMIT ?1)",
		[MAX_CHANNELS],
	)?;
	Ok(())
}

/// One invocation's view of the journal for one plugin. Reads advance a local position;
/// [`JournalSession::commit`] stores it once the host accepts the invocation.
pub struct JournalSession {
	path: PathBuf,
	plugin: String,
	connection: Option<Connection>,
	/// Committed cursor and the position read up to, once the first read ran.
	position: Option<(i64, i64)>,
	more: bool,
}

impl JournalSession {
	pub fn new(path: PathBuf, plugin: String) -> Self {
		Self {
			path,
			plugin,
			connection: None,
			position: None,
			more: false,
		}
	}

	/// Whether the last read left entries for another delivery.
	pub fn more(&self) -> bool {
		self.more
	}

	pub fn read(&mut self, max: u32) -> Reply {
		match self.try_read(max) {
			Ok(bytes) => Reply::Ok(bytes),
			Err(_) => Reply::Failed("Message journal is unavailable".into()),
		}
	}

	fn try_read(&mut self, max: u32) -> rusqlite::Result<Vec<u8>> {
		const EMPTY: &[u8] = br#"{"entries":[],"more":false}"#;
		if self.connection.is_none() {
			if !self.path.exists() {
				return Ok(EMPTY.to_vec());
			}
			let connection = Connection::open(&self.path)?;
			connection.busy_timeout(std::time::Duration::from_secs(5))?;
			self.connection = Some(connection);
		}
		let connection = self.connection.as_ref().expect("opened above");
		if self.position.is_none() {
			let cursor: Option<i64> = connection
				.query_row(
					"SELECT seq FROM cursors WHERE plugin = ?1",
					[&self.plugin],
					|row| row.get(0),
				)
				.optional()?;
			let Some(cursor) = cursor else {
				// Not registered yet; the recorder adds the cursor shortly.
				return Ok(EMPTY.to_vec());
			};
			self.position = Some((cursor, cursor));
		}
		let (committed, mut read) = self.position.expect("set above");
		let mut out = br#"{"entries":["#.to_vec();
		let mut count = 0_u32;
		self.more = false;
		// Appends one entry unless the batch is full; a full batch leaves the rest for more.
		let mut push = |out: &mut Vec<u8>, item: &[u8], more: &mut bool| {
			if count >= max || (count > 0 && out.len() + item.len() + 1 > MAX_BATCH_BYTES) {
				*more = true;
				return false;
			}
			if count > 0 {
				out.push(b',');
			}
			out.extend_from_slice(item);
			count += 1;
			true
		};
		let gap = |from: i64, to: i64, at: i64| {
			json!({
				"seq": to,
				"at_ms": at,
				"kind": "gap",
				"reason": "pruned",
				"from_seq": from,
				"to_seq": to,
			})
			.to_string()
			.into_bytes()
		};
		let mut statement = connection
			.prepare_cached("SELECT seq, at_ms, body FROM entries WHERE seq > ?1 ORDER BY seq")?;
		let mut rows = statement.query([read])?;
		let mut any = false;
		while let Some(row) = rows.next()? {
			any = true;
			let (seq, at, body): (i64, i64, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
			// Sequence numbers are never reused, so a hole after the cursor was pruned unread.
			if seq > read + 1 {
				if !push(&mut out, &gap(read + 1, seq - 1, at), &mut self.more) {
					break;
				}
				read = seq - 1;
			}
			let mut item = format!(r#"{{"seq":{seq},"at_ms":{at},"#).into_bytes();
			item.extend_from_slice(&body.as_bytes()[1..]);
			if !push(&mut out, &item, &mut self.more) {
				break;
			}
			read = seq;
		}
		drop(rows);
		drop(statement);
		if !any {
			let issued: i64 = connection.query_row(
				"SELECT coalesce((SELECT seq FROM sqlite_sequence WHERE name = 'entries'), 0)",
				[],
				|row| row.get(0),
			)?;
			if issued > read && push(&mut out, &gap(read + 1, issued, now_ms()), &mut self.more) {
				read = issued;
			}
		}
		out.extend_from_slice(format!(r#"],"more":{}}}"#, self.more).as_bytes());
		self.position = Some((committed, read));
		Ok(out)
	}

	/// Stores the position read up to. Does nothing when this invocation read nothing.
	pub fn commit(&mut self) -> Result<(), String> {
		let (Some(connection), Some((committed, read))) = (&self.connection, self.position) else {
			return Ok(());
		};
		if read <= committed {
			return Ok(());
		}
		connection
			.execute(
				"UPDATE cursors SET seq = ?1 WHERE plugin = ?2 AND seq < ?1",
				params![read, self.plugin],
			)
			.map(|_| ())
			.map_err(|_| "Message journal position could not be saved".to_string())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Directory(PathBuf);
	impl Directory {
		fn new() -> Self {
			let mut bytes = [0; 8];
			getrandom::fill(&mut bytes).unwrap();
			Self(std::env::temp_dir().join(format!(
				"serein-journal-{}",
				bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
			)))
		}
		fn path(&self) -> PathBuf {
			self.0.join(FILE_NAME)
		}
	}
	impl Drop for Directory {
		fn drop(&mut self) {
			let _ = std::fs::remove_dir_all(&self.0);
		}
	}

	fn body(text: &str) -> Record {
		Record::Entry {
			body: json!({ "kind": "create", "text": text }).to_string(),
			latest: None,
		}
	}

	fn write_all(path: &Path, consumers: Option<&[&str]>, records: Vec<Record>) {
		let mut connection = open(path).unwrap();
		let dropped = AtomicU64::new(0);
		let batch = records.into_iter().map(Command::Record).collect();
		write(
			&mut connection,
			batch,
			&dropped,
			consumers.map(|c| c.iter().map(|s| s.to_string()).collect()),
		)
		.unwrap();
	}

	fn read(path: &Path, plugin: &str, max: u32) -> (Value, JournalSession) {
		let mut session = JournalSession::new(path.to_path_buf(), plugin.into());
		let Reply::Ok(bytes) = session.read(max) else {
			panic!("journal read failed")
		};
		(serde_json::from_slice(&bytes).unwrap(), session)
	}

	fn kinds(batch: &Value) -> Vec<String> {
		batch["entries"]
			.as_array()
			.unwrap()
			.iter()
			.map(|entry| {
				entry["text"]
					.as_str()
					.or(entry["kind"].as_str())
					.unwrap()
					.to_owned()
			})
			.collect()
	}

	#[test]
	fn delivery_is_at_least_once_per_consumer_and_read_entries_are_pruned() {
		let directory = Directory::new();
		let path = directory.path();
		// Nothing is kept before anyone consumes the journal.
		write_all(&path, Some(&[]), vec![body("unheard")]);
		write_all(&path, Some(&["a", "b"]), vec![]);
		write_all(&path, None, vec![body("one"), body("two"), body("three")]);

		let (batch, session) = read(&path, "a", 2);
		assert_eq!(kinds(&batch), ["one", "two"]);
		assert_eq!(batch["more"], true);
		// Not committed: the next invocation sees the same entries.
		drop(session);
		let (batch, mut session) = read(&path, "a", 256);
		assert_eq!(kinds(&batch), ["one", "two", "three"]);
		assert!(!session.more());
		session.commit().unwrap();
		let (batch, _) = read(&path, "a", 256);
		assert!(kinds(&batch).is_empty());
		// "b" has its own cursor; nothing is pruned until it reads too.
		let (batch, mut b) = read(&path, "b", 256);
		assert_eq!(kinds(&batch), ["one", "two", "three"]);
		let seqs: Vec<i64> = batch["entries"]
			.as_array()
			.unwrap()
			.iter()
			.map(|e| e["seq"].as_i64().unwrap())
			.collect();
		assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1));
		b.commit().unwrap();
		write_all(&path, None, vec![]);
		let left: i64 = open(&path)
			.unwrap()
			.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))
			.unwrap();
		assert_eq!(left, 0);
		// A consumer added later starts after everything issued so far, without a false gap.
		write_all(&path, Some(&["a", "b", "c"]), vec![body("four")]);
		let (batch, _) = read(&path, "c", 256);
		assert_eq!(kinds(&batch), ["four"]);
		// Removing a consumer drops its cursor.
		write_all(&path, Some(&["a"]), vec![]);
		let (batch, _) = read(&path, "b", 256);
		assert!(kinds(&batch).is_empty());
	}

	#[test]
	fn losses_become_explicit_gaps() {
		let directory = Directory::new();
		let path = directory.path();
		write_all(&path, Some(&["a"]), vec![]);
		// A full queue reports how many records were dropped.
		let mut connection = open(&path).unwrap();
		let dropped = AtomicU64::new(7);
		write(
			&mut connection,
			vec![Command::Record(body("after"))],
			&dropped,
			None,
		)
		.unwrap();
		drop(connection);
		let (batch, _) = read(&path, "a", 256);
		assert_eq!(kinds(&batch), ["gap", "after"]);
		assert_eq!(batch["entries"][0]["reason"], "overflow");
		assert_eq!(batch["entries"][0]["dropped"], 7);

		// Entries pruned before a consumer read them become one gap with their range.
		let connection = open(&path).unwrap();
		connection
			.execute(
				"DELETE FROM entries WHERE seq IN (SELECT seq FROM entries ORDER BY seq LIMIT 1)",
				[],
			)
			.unwrap();
		drop(connection);
		let (batch, _) = read(&path, "a", 256);
		assert_eq!(kinds(&batch), ["gap", "after"]);
		assert_eq!(batch["entries"][0]["reason"], "pruned");
		assert_eq!(
			batch["entries"][0]["to_seq"],
			batch["entries"][1]["seq"].as_i64().unwrap() - 1
		);
	}

	#[test]
	fn new_sessions_flag_channels_that_moved_while_nobody_listened() {
		let directory = Directory::new();
		let path = directory.path();
		write_all(
			&path,
			Some(&["a"]),
			vec![Record::Entry {
				body: body("seen").into_body(),
				latest: Some((10, 100)),
			}],
		);
		let (_, mut session) = read(&path, "a", 256);
		session.commit().unwrap();
		write_all(&path, None, vec![Record::Seen(vec![(10, 150), (11, 5)])]);
		let (batch, mut session) = read(&path, "a", 256);
		assert_eq!(kinds(&batch), ["missed"]);
		assert_eq!(batch["entries"][0]["channel_id"], "10");
		assert_eq!(batch["entries"][0]["after_message_id"], "100");
		assert_eq!(batch["entries"][0]["latest_message_id"], "150");
		session.commit().unwrap();
		// Channel 11 now has a baseline; an unchanged session reports nothing.
		write_all(&path, None, vec![Record::Seen(vec![(10, 150), (11, 5)])]);
		let (batch, _) = read(&path, "a", 256);
		assert!(kinds(&batch).is_empty());
	}

	impl Record {
		fn into_body(self) -> String {
			match self {
				Record::Entry { body, .. } => body,
				Record::Seen(_) => unreachable!(),
			}
		}
	}

	#[test]
	fn edits_and_deletes_carry_the_version_the_timeline_held() {
		let mut state = test_support::demo_state();
		let mut original = test_support::message(1001, Id(20));
		original.content = "before".into();
		state.apply(client_core::Envelope {
			generation: state.generation,
			event: Event::Message(original.clone()),
		});
		let created = capture(&state, &Event::Message(original.clone()));
		let Record::Entry { body, latest } = &created[0] else {
			panic!()
		};
		assert_eq!(*latest, Some((20, 1001)));
		let created: Value = serde_json::from_str(body).unwrap();
		assert_eq!(created["message"]["content"], "before");

		let mut patch = MessagePatch {
			poll: Patch::Absent,
			id: Id(1001),
			channel: Id(20),
			content: Patch::Value("after".into()),
			edited: Patch::Value(1_700_000_000_000_000_000),
			sticker_items: Patch::Absent,
			flags: Patch::Absent,
			components: Patch::Absent,
			application_id: Patch::Absent,
			extra_content: Default::default(),
			reactions: Patch::Absent,
			mentions: Patch::Absent,
			embeds: Patch::Absent,
			embeds_suppressed: Patch::Absent,
			attachments: Patch::Absent,
		};
		let edit: Value =
			serde_json::from_str(&capture(&state, &Event::Patch(patch.clone()))[0].clone_body())
				.unwrap();
		assert_eq!(edit["content"], "after");
		assert_eq!(edit["edited_at_ms"], 1_700_000_000_000_i64);
		assert_eq!(edit["previous"]["content"], "before");
		// An embed refresh is not an edit.
		patch.content = Patch::Absent;
		patch.edited = Patch::Absent;
		patch.embeds = Patch::Value(Vec::new());
		assert!(capture(&state, &Event::Patch(patch)).is_empty());

		let deleted: Value = serde_json::from_str(
			&capture(
				&state,
				&Event::Delete {
					channel: Id(20),
					id: Id(1001),
				},
			)[0]
			.clone_body(),
		)
		.unwrap();
		assert_eq!(deleted["previous"]["content"], "before");
		// Messages the state never held are still journaled, without earlier text.
		let unknown = capture(
			&state,
			&Event::DeleteBulk {
				channel: Id(99),
				ids: vec![Id(5), Id(0), Id(6)],
			},
		);
		assert_eq!(unknown.len(), 2);
		let first: Value = serde_json::from_str(&unknown[0].clone_body()).unwrap();
		assert!(first["previous"].is_null() && first["bulk"] == true);
		// Oversized messages keep their text but drop attachment details.
		let mut huge = test_support::message(1002, Id(20));
		huge.attachments = (0..2_000)
			.map(|i| model::Attachment {
				id: Id(i),
				filename: "f".repeat(40),
				description: None,
				content_type: None,
				size: 1,
				media: Default::default(),
				spoiler: false,
				duration_ms: None,
				waveform: Vec::new(),
			})
			.collect();
		let huge: Value =
			serde_json::from_str(&capture(&state, &Event::Message(huge))[0].clone_body()).unwrap();
		assert_eq!(huge["message"]["truncated"], true);
		assert!(huge["message"]["attachments"].is_null());
	}

	impl Record {
		fn clone_body(&self) -> String {
			match self {
				Record::Entry { body, .. } => body.clone(),
				Record::Seen(_) => unreachable!(),
			}
		}
	}

	#[test]
	fn recorder_writes_in_the_background_and_flushes_on_drop() {
		let directory = Directory::new();
		let path = directory.path();
		let woke = Arc::new(AtomicU64::new(0));
		let counter = woke.clone();
		let recorder = Recorder::open(
			path.clone(),
			vec!["a".into()],
			Arc::new(move || {
				counter.fetch_add(1, Ordering::Relaxed);
			}),
		);
		recorder.record(vec![body("one"), body("two")]);
		drop(recorder);
		let (batch, _) = read(&path, "a", 256);
		assert_eq!(kinds(&batch), ["one", "two"]);
		assert!(woke.load(Ordering::Relaxed) >= 1);
	}
}
