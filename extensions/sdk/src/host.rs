//! Host functions imported from the `serein` Wasm module.
//!
//! Each function needs its capability in the manifest and the user's grant; a host rejects a
//! module that imports one without it. Only functions a plugin calls become imports, so
//! linking this module costs nothing until used. Outside `wasm32` every call reports
//! [`HostError::Unavailable`], so handlers stay testable natively.

/// Why a host function did not complete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostError {
	/// An argument was out of range, such as a buffer over [`MAX_RANDOM_BYTES`].
	Invalid,
	/// The host could not provide the service, or this is not a Wasm build.
	Unavailable,
}

/// Largest buffer one host `random_fill` call fills; [`fill_random`] splits larger ones.
pub const MAX_RANDOM_BYTES: usize = 4096;

#[cfg(target_arch = "wasm32")]
mod raw {
	#[link(wasm_import_module = "serein")]
	unsafe extern "C" {
		pub fn clock_now_ms() -> i64;
		pub fn random_fill(pointer: *mut u8, length: i32) -> i32;
		pub fn db_execute(pointer: *const u8, length: i32) -> i32;
		pub fn result_read(pointer: *mut u8, capacity: i32) -> i32;
		pub fn journal_read(max: i32) -> i32;
		pub fn https_request(pointer: *const u8, length: i32) -> i32;
		pub fn broadcast(pointer: *const u8, length: i32) -> i32;
		pub fn has_subscribers(pointer: *const u8, length: i32) -> i32;
		pub fn topic_read(max: i32) -> i32;
		pub fn file_request(pointer: *const u8, length: i32) -> i32;
		pub fn file_write(
			pointer: *const u8,
			length: i32,
			data: *const u8,
			data_length: i32,
		) -> i32;
	}
}

fn status(code: i32) -> Result<(), HostError> {
	match code {
		0 => Ok(()),
		-1 => Err(HostError::Invalid),
		_ => Err(HostError::Unavailable),
	}
}

/// Current Unix time in milliseconds. Requires the `clock` capability.
pub fn now_ms() -> Result<i64, HostError> {
	#[cfg(target_arch = "wasm32")]
	{
		// SAFETY: a host function with no arguments and no memory access.
		let ms = unsafe { raw::clock_now_ms() };
		if ms >= 0 {
			Ok(ms)
		} else {
			Err(HostError::Unavailable)
		}
	}
	#[cfg(not(target_arch = "wasm32"))]
	Err(HostError::Unavailable)
}

/// Fills `out` with secure random bytes. Requires the `random` capability.
pub fn fill_random(out: &mut [u8]) -> Result<(), HostError> {
	for chunk in out.chunks_mut(MAX_RANDOM_BYTES) {
		#[cfg(target_arch = "wasm32")]
		// SAFETY: the host writes at most `chunk.len()` bytes into this exclusive buffer.
		status(unsafe { raw::random_fill(chunk.as_mut_ptr(), chunk.len() as i32) })?;
		#[cfg(not(target_arch = "wasm32"))]
		{
			let _ = chunk;
			status(-2)?;
		}
	}
	Ok(())
}

/// The plugin's private SQLite database. Requires the `database` capability.
///
/// Every statement in one invocation runs in a single transaction that the host commits only
/// when the invocation's output is accepted; an error, trap or rejected output discards it.
/// Transaction statements are refused, but `SAVEPOINT` works. Only `user_version` and schema
/// introspection pragmas are allowed; ATTACH, temporary tables and virtual tables are refused.
/// The file holds at most 64 MiB, one reply at most 256 KiB, and SQLite work per invocation
/// is budgeted.
pub mod db {
	use super::HostError;

	/// Largest request [`execute`] sends; a larger one fails with [`HostError::Invalid`].
	pub const MAX_REQUEST_BYTES: usize = 256 * 1024;
	/// Largest successful reply; a larger result fails with [`DbError::Sql`].
	pub const MAX_REPLY_BYTES: usize = 256 * 1024;
	#[cfg(target_arch = "wasm32")]
	const MAX_ERROR_BYTES: usize = 1024;

	/// A SQLite value. Blobs travel as base64 in `{"blob": "..."}`.
	#[derive(Clone, Debug, PartialEq)]
	pub enum Value {
		Null,
		Integer(i64),
		Real(f64),
		Text(String),
		Blob(Vec<u8>),
	}

	impl From<i64> for Value {
		fn from(value: i64) -> Self {
			Self::Integer(value)
		}
	}
	impl From<bool> for Value {
		fn from(value: bool) -> Self {
			Self::Integer(i64::from(value))
		}
	}
	impl From<f64> for Value {
		fn from(value: f64) -> Self {
			Self::Real(value)
		}
	}
	impl From<&str> for Value {
		fn from(value: &str) -> Self {
			Self::Text(value.into())
		}
	}
	impl From<String> for Value {
		fn from(value: String) -> Self {
			Self::Text(value)
		}
	}
	impl From<Vec<u8>> for Value {
		fn from(value: Vec<u8>) -> Self {
			Self::Blob(value)
		}
	}
	impl<T: Into<Value>> From<Option<T>> for Value {
		fn from(value: Option<T>) -> Self {
			value.map_or(Self::Null, Into::into)
		}
	}

	impl Value {
		pub fn as_i64(&self) -> Option<i64> {
			match self {
				Self::Integer(value) => Some(*value),
				_ => None,
			}
		}
		pub fn as_f64(&self) -> Option<f64> {
			match self {
				Self::Real(value) => Some(*value),
				Self::Integer(value) => Some(*value as f64),
				_ => None,
			}
		}
		pub fn as_str(&self) -> Option<&str> {
			match self {
				Self::Text(value) => Some(value),
				_ => None,
			}
		}
		pub fn as_blob(&self) -> Option<&[u8]> {
			match self {
				Self::Blob(value) => Some(value),
				_ => None,
			}
		}

		fn to_json(&self) -> serde_json::Value {
			match self {
				Self::Null => serde_json::Value::Null,
				Self::Integer(value) => (*value).into(),
				Self::Real(value) => serde_json::Number::from_f64(*value)
					.map_or(serde_json::Value::Null, serde_json::Value::Number),
				Self::Text(value) => value.as_str().into(),
				Self::Blob(bytes) => serde_json::json!({ "blob": base64::encode(bytes) }),
			}
		}

		fn from_json(value: serde_json::Value) -> Option<Self> {
			Some(match value {
				serde_json::Value::Null => Self::Null,
				serde_json::Value::Number(number) => match number.as_i64() {
					Some(integer) => Self::Integer(integer),
					None => Self::Real(number.as_f64()?),
				},
				serde_json::Value::String(text) => Self::Text(text),
				serde_json::Value::Object(object) => {
					Self::Blob(base64::decode(object.get("blob")?.as_str()?)?)
				}
				_ => return None,
			})
		}
	}

	/// Result of one statement.
	#[derive(Clone, Debug, Default, PartialEq)]
	pub struct Rows {
		pub columns: Vec<String>,
		pub rows: Vec<Vec<Value>>,
		/// Rows the statement changed; zero for read-only statements.
		pub changes: u64,
		/// Row ID of the last insert on this connection; zero for read-only statements.
		pub last_insert_rowid: i64,
	}

	/// Why a database request failed.
	#[derive(Clone, Debug, PartialEq, Eq)]
	pub enum DbError {
		/// The host refused the call or provides no database.
		Host(HostError),
		/// SQLite or the host's sandbox rejected the statement; at most 1 KiB of text.
		Sql(String),
		/// The host's reply could not be decoded.
		Reply,
	}

	/// Runs one SQL statement with `?` parameters.
	pub fn execute(sql: &str, params: &[Value]) -> Result<Rows, DbError> {
		let request = serde_json::json!({
			"sql": sql,
			"params": params.iter().map(Value::to_json).collect::<Vec<_>>(),
		});
		let request = serde_json::to_vec(&request).map_err(|_| DbError::Reply)?;
		if request.len() > MAX_REQUEST_BYTES {
			return Err(DbError::Host(HostError::Invalid));
		}
		parse(&call(&request)?)
	}

	fn parse(reply: &[u8]) -> Result<Rows, DbError> {
		#[derive(serde::Deserialize)]
		struct Wire {
			columns: Vec<String>,
			rows: Vec<Vec<serde_json::Value>>,
			changes: u64,
			last_insert_rowid: i64,
		}
		let wire: Wire = serde_json::from_slice(reply).map_err(|_| DbError::Reply)?;
		Ok(Rows {
			columns: wire.columns,
			rows: wire
				.rows
				.into_iter()
				.map(|row| row.into_iter().map(Value::from_json).collect())
				.collect::<Option<_>>()
				.ok_or(DbError::Reply)?,
			changes: wire.changes,
			last_insert_rowid: wire.last_insert_rowid,
		})
	}

	#[cfg(target_arch = "wasm32")]
	fn call(request: &[u8]) -> Result<Vec<u8>, DbError> {
		// SAFETY: the host reads `request.len()` bytes from this live buffer.
		let code = unsafe { super::raw::db_execute(request.as_ptr(), request.len() as i32) };
		let (capacity, failed) = match code {
			0.. => (code as usize, false),
			-3 => (MAX_ERROR_BYTES, true),
			-1 => return Err(DbError::Host(HostError::Invalid)),
			_ => return Err(DbError::Host(HostError::Unavailable)),
		};
		let mut reply = vec![0_u8; capacity];
		// SAFETY: the host writes at most `capacity` bytes into this exclusive buffer.
		let read = unsafe { super::raw::result_read(reply.as_mut_ptr(), capacity as i32) };
		if read < 0 {
			return Err(DbError::Reply);
		}
		reply.truncate(read as usize);
		if failed {
			Err(DbError::Sql(String::from_utf8_lossy(&reply).into_owned()))
		} else {
			Ok(reply)
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	fn call(_: &[u8]) -> Result<Vec<u8>, DbError> {
		Err(DbError::Host(HostError::Unavailable))
	}

	/// Standard padded base64, matching the host's blob encoding.
	mod base64 {
		const ALPHABET: &[u8; 64] =
			b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

		pub fn encode(bytes: &[u8]) -> String {
			let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
			for chunk in bytes.chunks(3) {
				let n = chunk
					.iter()
					.enumerate()
					.fold(0_u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
				for i in 0..4 {
					if i <= chunk.len() {
						out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
					} else {
						out.push('=');
					}
				}
			}
			out
		}

		pub fn decode(text: &str) -> Option<Vec<u8>> {
			let text = text.as_bytes();
			if !text.len().is_multiple_of(4) {
				return None;
			}
			let chunks = text.len() / 4;
			let mut out = Vec::with_capacity(chunks * 3);
			for (index, chunk) in text.chunks(4).enumerate() {
				let padding = chunk.iter().rev().take_while(|b| **b == b'=').count();
				if padding > 2 || padding > 0 && index + 1 != chunks {
					return None;
				}
				let mut n = 0_u32;
				for (i, byte) in chunk[..4 - padding].iter().enumerate() {
					let digit = ALPHABET.iter().position(|a| a == byte)? as u32;
					n |= digit << (18 - 6 * i);
				}
				out.extend_from_slice(&n.to_be_bytes()[1..4 - padding]);
			}
			Some(out)
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn replies_and_blobs_match_the_host_wire() {
			for bytes in [
				&b""[..],
				b"\x00",
				b"\x00\x01",
				b"\x00\x01\xff",
				b"hello world",
			] {
				assert_eq!(
					base64::decode(&base64::encode(bytes)).as_deref(),
					Some(bytes)
				);
			}
			assert_eq!(base64::encode(b"\x00\x01\xff"), "AAH/");
			assert!(base64::decode("AA=A").is_none() && base64::decode("AAA").is_none());
			let rows = parse(
				br#"{"columns":["a","b","c","d"],"rows":[[1,1.5,"x",{"blob":"AAH/"}],[null,null,null,null]],"changes":0,"last_insert_rowid":0}"#,
			)
			.unwrap();
			assert_eq!(
				rows.rows[0],
				[
					Value::Integer(1),
					Value::Real(1.5),
					"x".into(),
					Value::Blob(vec![0, 1, 255])
				]
			);
			assert_eq!(
				rows.rows[1],
				[Value::Null, Value::Null, Value::Null, Value::Null]
			);
			assert!(
				parse(br#"{"columns":[],"rows":[[[1]]],"changes":0,"last_insert_rowid":0}"#)
					.is_err()
			);
			assert_eq!(
				execute("SELECT 1", &[]),
				Err(DbError::Host(HostError::Unavailable))
			);
		}
	}
}

/// The account's message journal. Requires the `message_journal` capability and runs in a
/// `journal` action, which the host schedules whenever entries are waiting.
///
/// Each [`read`] returns entries after this plugin's position. The position moves only when
/// the invocation succeeds and its output is accepted, right after the plugin database
/// commits, so storing entries with [`super::db`] in the same invocation never loses one:
/// a failed invocation sees the same entries again. Entries are therefore delivered at least
/// once; deduplicate by `seq` or by message ID.
pub mod journal {
	use super::HostError;
	use serde::Deserialize;
	use serde_json::Value;

	/// Most entries one [`read`] returns.
	pub const MAX_ENTRIES: u32 = 256;

	/// One batch of entries, in sequence order.
	#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
	pub struct Batch {
		pub entries: Vec<Entry>,
		/// Entries remain after this batch; the host schedules another delivery.
		#[serde(default)]
		pub more: bool,
	}

	/// A journal entry: its position, when the host recorded it, and what happened.
	#[derive(Clone, Debug, Deserialize, PartialEq)]
	pub struct Entry {
		pub seq: i64,
		pub at_ms: i64,
		#[serde(flatten)]
		pub kind: EntryKind,
	}

	/// What a journal entry records. Unknown kinds from newer hosts are kept as [`EntryKind::Other`].
	#[derive(Clone, Debug, Deserialize, PartialEq)]
	#[serde(tag = "kind", rename_all = "snake_case")]
	pub enum EntryKind {
		/// A new message.
		Create {
			channel_id: String,
			#[serde(default)]
			guild_id: Option<String>,
			message: Message,
		},
		/// An edit. Changed fields are present; `previous` is the version the host held,
		/// when it held one.
		Update {
			channel_id: String,
			#[serde(default)]
			guild_id: Option<String>,
			message_id: String,
			#[serde(default)]
			content: Option<String>,
			#[serde(default)]
			edited_at_ms: Option<i64>,
			#[serde(default)]
			attachments: Option<Vec<Attachment>>,
			#[serde(default)]
			previous: Option<Message>,
		},
		/// A deletion; `bulk` for moderator purges. `previous` as for updates.
		Delete {
			channel_id: String,
			#[serde(default)]
			guild_id: Option<String>,
			message_id: String,
			#[serde(default)]
			bulk: bool,
			#[serde(default)]
			previous: Option<Message>,
		},
		/// Entries were lost: `overflow` (the host could not keep up; `dropped` records) or
		/// `pruned` (the journal's bounds removed `from_seq..=to_seq` before this plugin read them).
		Gap {
			reason: String,
			#[serde(default)]
			dropped: Option<u64>,
			#[serde(default)]
			from_seq: Option<i64>,
			#[serde(default)]
			to_seq: Option<i64>,
		},
		/// A new gateway session began; events while disconnected or closed are not in the journal.
		SessionStarted,
		/// The channel's latest message moved past the last one journaled while nobody listened.
		Missed {
			channel_id: String,
			after_message_id: String,
			latest_message_id: String,
		},
		/// A channel was created or its details changed.
		ChannelChanged { channel_id: String },
		/// Which conversations the account can read may have changed.
		AccessChanged,
		#[serde(other)]
		Other,
	}

	/// The archived form of a message.
	#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
	pub struct Message {
		pub id: String,
		pub author: Author,
		pub content: String,
		pub created_at_ms: i64,
		#[serde(default)]
		pub edited_at_ms: Option<i64>,
		/// Discord message type.
		#[serde(default, rename = "type")]
		pub kind: u8,
		#[serde(default)]
		pub flags: u64,
		#[serde(default)]
		pub reply_to: Option<String>,
		#[serde(default)]
		pub forwarded: bool,
		/// Number of embeds; their contents are not journaled.
		#[serde(default)]
		pub embeds: u32,
		#[serde(default)]
		pub stickers: u32,
		/// Absent when `truncated`: the message was too large to journal its attachments.
		#[serde(default)]
		pub attachments: Option<Vec<Attachment>>,
		#[serde(default)]
		pub truncated: bool,
	}

	#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
	pub struct Author {
		pub id: String,
		pub name: String,
		#[serde(default)]
		pub nick: Option<String>,
		#[serde(default)]
		pub bot: bool,
		#[serde(default)]
		pub webhook: bool,
	}

	#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
	pub struct Attachment {
		pub id: String,
		pub filename: String,
		#[serde(default)]
		pub content_type: Option<String>,
		#[serde(default)]
		pub size: u64,
		#[serde(default)]
		pub url: Option<String>,
		#[serde(default)]
		pub proxy_url: Option<String>,
	}

	/// Why a journal read failed.
	#[derive(Clone, Debug, PartialEq, Eq)]
	pub enum JournalError {
		Host(HostError),
		/// The host could not read the journal right now.
		Failed(String),
		/// The host's reply could not be decoded.
		Reply,
	}

	/// Reads up to `max` (1 to [`MAX_ENTRIES`]) entries after this plugin's position.
	pub fn read(max: u32) -> Result<Batch, JournalError> {
		if !(1..=MAX_ENTRIES).contains(&max) {
			return Err(JournalError::Host(HostError::Invalid));
		}
		parse(&call(max)?)
	}

	fn parse(reply: &[u8]) -> Result<Batch, JournalError> {
		serde_json::from_slice(reply).map_err(|_| JournalError::Reply)
	}

	/// Decodes an entry's raw JSON, for tools that read entries outside a plugin.
	pub fn decode(value: Value) -> Option<Entry> {
		serde_json::from_value(value).ok()
	}

	#[cfg(target_arch = "wasm32")]
	fn call(max: u32) -> Result<Vec<u8>, JournalError> {
		// SAFETY: a host call with no pointer arguments.
		let code = unsafe { super::raw::journal_read(max as i32) };
		let (capacity, failed) = match code {
			0.. => (code as usize, false),
			-3 => (1024, true),
			-1 => return Err(JournalError::Host(HostError::Invalid)),
			_ => return Err(JournalError::Host(HostError::Unavailable)),
		};
		let mut reply = vec![0_u8; capacity];
		// SAFETY: the host writes at most `capacity` bytes into this exclusive buffer.
		let read = unsafe { super::raw::result_read(reply.as_mut_ptr(), capacity as i32) };
		if read < 0 {
			return Err(JournalError::Reply);
		}
		reply.truncate(read as usize);
		if failed {
			Err(JournalError::Failed(
				String::from_utf8_lossy(&reply).into_owned(),
			))
		} else {
			Ok(reply)
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	fn call(_: u32) -> Result<Vec<u8>, JournalError> {
		Err(JournalError::Host(HostError::Unavailable))
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn host_entries_decode_including_unknown_kinds() {
			let batch = parse(
				br#"{"entries":[
				{"seq":1,"at_ms":5,"kind":"create","channel_id":"2","guild_id":null,"message":{"id":"3","author":{"id":"4","name":"a","nick":null,"bot":false,"webhook":false},"content":"hi","created_at_ms":9,"edited_at_ms":null,"type":0,"flags":0,"reply_to":null,"forwarded":false,"embeds":0,"stickers":0,"attachments":[]}},
				{"seq":2,"at_ms":6,"kind":"delete","channel_id":"2","guild_id":"7","message_id":"3","bulk":false,"previous":null},
				{"seq":3,"at_ms":6,"kind":"gap","reason":"pruned","from_seq":3,"to_seq":3},
				{"seq":4,"at_ms":7,"kind":"from_a_newer_host","x":1}
			],"more":true}"#,
			)
			.unwrap();
			assert!(batch.more);
			assert!(
				matches!(&batch.entries[0].kind, EntryKind::Create { message, .. } if message.content == "hi")
			);
			assert!(matches!(
				&batch.entries[1].kind,
				EntryKind::Delete { previous: None, .. }
			));
			assert!(matches!(
				&batch.entries[2].kind,
				EntryKind::Gap {
					to_seq: Some(3),
					..
				}
			));
			assert_eq!(batch.entries[3].kind, EntryKind::Other);
			assert_eq!(read(0), Err(JournalError::Host(HostError::Invalid)));
			assert_eq!(read(1), Err(JournalError::Host(HostError::Unavailable)));
		}
	}
}

/// HTTPS requests to the origins in the manifest's `network` list. Requires the `network`
/// capability.
///
/// Each [`send`] is one request and one complete response; the invocation waits for it.
/// The host resolves names itself and refuses non-public addresses, follows redirects only
/// to declared origins (others come back as the 3xx response), sends no cookies and does not
/// decompress bodies. An invocation may make 32 requests within 30 seconds; one request
/// takes at most 15 seconds, sends at most 192 KiB and receives at most 160 KiB.
///
/// Secrets the manifest declares are written as `{{secret.<id>}}` in the URL, a header
/// value or a text body; the host fills in the user's value for the secret's origins and
/// replaces any echo of it in the response with `[secret]`, so the plugin never reads it.
pub mod https {
	use super::HostError;
	use serde::{Deserialize, Serialize};

	/// One request. `body` and `body_base64` are exclusive.
	#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
	pub struct Request {
		pub method: String,
		pub url: String,
		#[serde(skip_serializing_if = "Vec::is_empty")]
		pub headers: Vec<(String, String)>,
		#[serde(skip_serializing_if = "Option::is_none")]
		pub body: Option<String>,
		#[serde(skip_serializing_if = "Option::is_none")]
		pub body_base64: Option<String>,
		/// At most 15,000; the host's default is 10,000.
		#[serde(skip_serializing_if = "Option::is_none")]
		pub timeout_ms: Option<u64>,
	}

	impl Request {
		pub fn get(url: impl Into<String>) -> Self {
			Self {
				method: "GET".into(),
				url: url.into(),
				..Default::default()
			}
		}
		/// A POST with a JSON body and `Content-Type: application/json`.
		pub fn post_json(url: impl Into<String>, body: &impl Serialize) -> Self {
			Self {
				method: "POST".into(),
				url: url.into(),
				headers: vec![("Content-Type".into(), "application/json".into())],
				body: serde_json::to_string(body).ok(),
				..Default::default()
			}
		}
		pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
			self.headers.push((name.into(), value.into()));
			self
		}
	}

	/// A complete response. Text bodies arrive in `body`, others in `body_base64`.
	#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
	pub struct Response {
		pub status: u16,
		/// The URL that answered, after declared redirects.
		pub url: String,
		#[serde(default)]
		pub redirected: bool,
		#[serde(default)]
		pub headers: Vec<(String, String)>,
		#[serde(default)]
		pub body: Option<String>,
		#[serde(default)]
		pub body_base64: Option<String>,
	}

	impl Response {
		/// The first header with this name, ignoring case.
		pub fn header(&self, name: &str) -> Option<&str> {
			self.headers
				.iter()
				.find(|(key, _)| key.eq_ignore_ascii_case(name))
				.map(|(_, value)| value.as_str())
		}
		/// Parses a text body as JSON.
		pub fn json<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
			serde_json::from_str(self.body.as_deref()?).ok()
		}
	}

	/// Why a request did not produce a response.
	#[derive(Clone, Debug, PartialEq, Eq)]
	pub enum HttpsError {
		Host(HostError),
		/// The host refused or could not complete the request: an undeclared origin, a
		/// private address, a timeout, a limit or a connection failure. At most 1 KiB.
		Failed(String),
		/// The host's reply could not be decoded.
		Reply,
	}

	/// Sends `request` and waits for its response.
	pub fn send(request: &Request) -> Result<Response, HttpsError> {
		let bytes = serde_json::to_vec(request).map_err(|_| HttpsError::Reply)?;
		if bytes.len() > 256 * 1024 {
			return Err(HttpsError::Host(HostError::Invalid));
		}
		serde_json::from_slice(&call(&bytes)?).map_err(|_| HttpsError::Reply)
	}

	#[cfg(target_arch = "wasm32")]
	fn call(request: &[u8]) -> Result<Vec<u8>, HttpsError> {
		// SAFETY: the host reads `request.len()` bytes from this live buffer.
		let code = unsafe { super::raw::https_request(request.as_ptr(), request.len() as i32) };
		let (capacity, failed) = match code {
			0.. => (code as usize, false),
			-3 => (1024, true),
			-1 => return Err(HttpsError::Host(HostError::Invalid)),
			_ => return Err(HttpsError::Host(HostError::Unavailable)),
		};
		let mut reply = vec![0_u8; capacity];
		// SAFETY: the host writes at most `capacity` bytes into this exclusive buffer.
		let read = unsafe { super::raw::result_read(reply.as_mut_ptr(), capacity as i32) };
		if read < 0 {
			return Err(HttpsError::Reply);
		}
		reply.truncate(read as usize);
		if failed {
			Err(HttpsError::Failed(
				String::from_utf8_lossy(&reply).into_owned(),
			))
		} else {
			Ok(reply)
		}
	}

	#[cfg(not(target_arch = "wasm32"))]
	fn call(_: &[u8]) -> Result<Vec<u8>, HttpsError> {
		Err(HttpsError::Host(HostError::Unavailable))
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn requests_and_responses_match_the_host_wire() {
			let request =
				Request::post_json("https://api.example.org/v1", &serde_json::json!({ "q": 1 }))
					.header("Authorization", "Bearer x");
			assert_eq!(
				serde_json::to_value(&request).unwrap(),
				serde_json::json!({
					"method": "POST",
					"url": "https://api.example.org/v1",
					"headers": [["Content-Type", "application/json"], ["Authorization", "Bearer x"]],
					"body": "{\"q\":1}",
				})
			);
			let response: Response = serde_json::from_str(
				r#"{"status":200,"url":"https://api.example.org/v1","redirected":false,"headers":[["content-type","application/json"]],"body":"{\"ok\":true}"}"#,
			)
			.unwrap();
			assert_eq!(response.header("Content-Type"), Some("application/json"));
			assert_eq!(response.json::<serde_json::Value>().unwrap()["ok"], true);
			assert_eq!(
				send(&Request::get("https://api.example.org")),
				Err(HttpsError::Host(HostError::Unavailable))
			);
		}
	}
}

/// Broadcasts between plugins on versioned topics such as `serein.captions/1`.
///
/// A provider lists the topics it may [`broadcast`] in the manifest's `broadcasts`; a
/// subscriber lists what it wants in `subscribes` and declares one `topic` action, where it
/// calls [`read`]. Nothing flows until the user approves that provider, subscriber and
/// topic. Broadcasts with no approved subscriber are dropped at once, and nothing is kept
/// for subscribers that connect later. Messages leave a subscriber's queue only when its
/// invocation succeeds; a full queue reports a [`Item::Gap`] with how many were lost.
pub mod topics {
	use super::HostError;
	use serde::{Deserialize, Serialize};

	/// Largest serialized payload of one broadcast.
	pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024;
	/// Most messages one [`read`] returns.
	pub const MAX_READ: u32 = 64;

	/// One delivery batch, oldest first.
	#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
	pub struct Batch {
		pub messages: Vec<Item>,
		#[serde(default)]
		pub more: bool,
	}

	#[derive(Clone, Debug, Deserialize, PartialEq)]
	#[serde(tag = "kind", rename_all = "snake_case")]
	pub enum Item {
		Message {
			seq: u64,
			provider: String,
			topic: String,
			at_ms: i64,
			payload: serde_json::Value,
		},
		/// `dropped` messages from `provider` on `topic` were lost here because this
		/// subscriber's queue was full.
		Gap {
			seq: u64,
			provider: String,
			topic: String,
			dropped: u64,
		},
		#[serde(other)]
		Other,
	}

	#[derive(Clone, Debug, PartialEq, Eq)]
	pub enum TopicError {
		Host(HostError),
		/// The host's reply could not be decoded.
		Reply,
	}

	/// Sends `payload` on `topic`; the number of approved subscribers that received it.
	/// [`HostError::Invalid`] for an undeclared topic or an oversized payload,
	/// [`HostError::Unavailable`] once this invocation sent 32.
	pub fn broadcast(topic: &str, payload: &impl Serialize) -> Result<u32, TopicError> {
		#[derive(Serialize)]
		struct Request<'a, T: Serialize> {
			topic: &'a str,
			payload: &'a T,
		}
		let request =
			serde_json::to_vec(&Request { topic, payload }).map_err(|_| TopicError::Reply)?;
		if request.len() > MAX_PAYLOAD_BYTES + 1024 {
			return Err(TopicError::Host(HostError::Invalid));
		}
		code(raw_broadcast(&request)).map(|reached| reached as u32)
	}

	/// Whether `topic`, which the manifest must declare in `broadcasts`, has an approved
	/// subscriber; check before expensive work.
	pub fn has_subscribers(topic: &str) -> bool {
		raw_has_subscribers(topic.as_bytes()) == 1
	}

	/// Reads up to `max` (1 to [`MAX_READ`]) delivered messages in a `topic` action.
	pub fn read(max: u32) -> Result<Batch, TopicError> {
		if !(1..=MAX_READ).contains(&max) {
			return Err(TopicError::Host(HostError::Invalid));
		}
		serde_json::from_slice(&raw_read(max)?).map_err(|_| TopicError::Reply)
	}

	fn code(code: i32) -> Result<i32, TopicError> {
		match code {
			0.. => Ok(code),
			-1 => Err(TopicError::Host(HostError::Invalid)),
			_ => Err(TopicError::Host(HostError::Unavailable)),
		}
	}

	#[cfg(target_arch = "wasm32")]
	fn raw_broadcast(request: &[u8]) -> i32 {
		// SAFETY: the host reads `request.len()` bytes from this live buffer.
		unsafe { super::raw::broadcast(request.as_ptr(), request.len() as i32) }
	}
	#[cfg(target_arch = "wasm32")]
	fn raw_has_subscribers(topic: &[u8]) -> i32 {
		// SAFETY: the host reads `topic.len()` bytes from this live buffer.
		unsafe { super::raw::has_subscribers(topic.as_ptr(), topic.len() as i32) }
	}
	#[cfg(target_arch = "wasm32")]
	fn raw_read(max: u32) -> Result<Vec<u8>, TopicError> {
		// SAFETY: a host call with no pointer arguments.
		let length = code(unsafe { super::raw::topic_read(max as i32) })? as usize;
		let mut reply = vec![0_u8; length];
		// SAFETY: the host writes at most `length` bytes into this exclusive buffer.
		let read = unsafe { super::raw::result_read(reply.as_mut_ptr(), length as i32) };
		if read < 0 {
			return Err(TopicError::Reply);
		}
		reply.truncate(read as usize);
		Ok(reply)
	}

	#[cfg(not(target_arch = "wasm32"))]
	fn raw_broadcast(_: &[u8]) -> i32 {
		-2
	}
	#[cfg(not(target_arch = "wasm32"))]
	fn raw_has_subscribers(_: &[u8]) -> i32 {
		-2
	}
	#[cfg(not(target_arch = "wasm32"))]
	fn raw_read(_: u32) -> Result<Vec<u8>, TopicError> {
		Err(TopicError::Host(HostError::Unavailable))
	}

	#[cfg(test)]
	mod tests {
		use super::*;

		#[test]
		fn deliveries_decode_messages_gaps_and_future_kinds() {
			let batch: Batch = serde_json::from_str(
				r#"{"messages":[
				{"kind":"message","seq":3,"provider":"captioner","topic":"serein.captions/1","at_ms":9,"payload":{"text":"hi"}},
				{"kind":"gap","seq":4,"provider":"captioner","topic":"serein.captions/1","dropped":5},
				{"kind":"from_a_newer_host"}
			],"more":false}"#,
			)
			.unwrap();
			assert!(
				matches!(&batch.messages[0], Item::Message { payload, .. } if payload["text"] == "hi")
			);
			assert!(matches!(batch.messages[1], Item::Gap { dropped: 5, .. }));
			assert_eq!(batch.messages[2], Item::Other);
			assert!(!has_subscribers("serein.captions/1"));
			assert_eq!(
				broadcast("serein.captions/1", &1),
				Err(TopicError::Host(HostError::Unavailable))
			);
			assert_eq!(read(0), Err(TopicError::Host(HostError::Invalid)));
		}
	}
}

/// Files in this plugin's private folder and in folders or files the user picked for it.
/// Requires the `files` capability.
///
/// Every call names a root, [`PRIVATE`](files::PRIVATE) or a picked root id from
/// [`handles`](files::handles), and a relative `/`-separated path; the host refuses `..`,
/// absolute paths and symbolic links. The private folder counts against the plugin's data
/// limit. Changes apply at once, even if the invocation later fails. Ask the user for a
/// folder or file with [`crate::FilePick`].
pub mod files {
	use super::HostError;
	use serde::{Deserialize, Serialize};

	/// The root naming the plugin's own folder.
	pub const PRIVATE: &str = "private";
	/// Largest data one host write sends; [`write`] splits larger data.
	pub const MAX_WRITE_BYTES: usize = 1024 * 1024;
	/// Largest data one [`read`] returns.
	pub const MAX_READ_BYTES: u32 = 256 * 1024;

	#[derive(Clone, Debug, PartialEq, Eq)]
	pub enum FileError {
		Host(HostError),
		/// The host refused or could not do it, such as a missing file, a read-only root or
		/// the data limit; at most 1 KiB.
		Failed(String),
		/// The host's reply could not be decoded.
		Reply,
	}

	#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
	pub struct Entry {
		pub name: String,
		pub folder: bool,
		pub size: u64,
		#[serde(default)]
		pub modified_ms: Option<i64>,
	}

	#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
	pub struct Listing {
		pub entries: Vec<Entry>,
		pub more: bool,
	}

	#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
	pub struct Stat {
		pub exists: bool,
		#[serde(default)]
		pub folder: bool,
		#[serde(default)]
		pub size: u64,
		#[serde(default)]
		pub modified_ms: Option<i64>,
	}

	#[derive(Deserialize)]
	struct Handles {
		handles: Vec<crate::FileHandleInfo>,
	}

	/// The folders and files the user gave this plugin.
	pub fn handles() -> Result<Vec<crate::FileHandleInfo>, FileError> {
		decode::<Handles>(request(&serde_json::json!({"op": "handles"}))?).map(|h| h.handles)
	}
	/// Up to `limit` (at most 512) entries of a folder after the first `offset`, by name.
	pub fn list(root: &str, path: &str, offset: u32, limit: u32) -> Result<Listing, FileError> {
		decode(request(&serde_json::json!({
			"op": "list", "root": root, "path": path, "offset": offset, "limit": limit,
		}))?)
	}
	pub fn stat(root: &str, path: &str) -> Result<Stat, FileError> {
		decode(request(
			&serde_json::json!({"op": "stat", "root": root, "path": path}),
		)?)
	}
	/// Up to `length` bytes (at most [`MAX_READ_BYTES`]) from `offset`.
	pub fn read(root: &str, path: &str, offset: u64, length: u32) -> Result<Vec<u8>, FileError> {
		request(&serde_json::json!({
			"op": "read", "root": root, "path": path, "offset": offset, "length": length,
		}))
	}
	/// Creates a folder and its missing parents.
	pub fn create_folder(root: &str, path: &str) -> Result<(), FileError> {
		request(&serde_json::json!({"op": "create_folder", "root": root, "path": path})).map(drop)
	}
	/// Removes a file or an empty folder.
	pub fn delete(root: &str, path: &str) -> Result<(), FileError> {
		request(&serde_json::json!({"op": "delete", "root": root, "path": path})).map(drop)
	}
	/// Moves within one root, never over an existing entry.
	pub fn rename(root: &str, path: &str, to: &str) -> Result<(), FileError> {
		request(&serde_json::json!({"op": "rename", "root": root, "path": path, "to": to}))
			.map(drop)
	}
	/// Writes `data` at `offset`, or appends with `None`; `truncate` empties the file first.
	pub fn write(
		root: &str,
		path: &str,
		offset: Option<u64>,
		truncate: bool,
		data: &[u8],
	) -> Result<(), FileError> {
		let mut at = offset;
		let mut truncate = truncate;
		let empty: &[u8] = &[];
		let chunks = data
			.chunks(MAX_WRITE_BYTES)
			.chain(data.is_empty().then_some(empty));
		for chunk in chunks {
			let header = Header {
				root,
				path,
				offset: at,
				truncate,
			};
			let header = serde_json::to_vec(&header).map_err(|_| FileError::Reply)?;
			call_write(&header, chunk)?;
			at = at.map(|at| at + chunk.len() as u64);
			truncate = false;
		}
		Ok(())
	}
	/// Appends `data` to the end of the file, creating it.
	pub fn append(root: &str, path: &str, data: &[u8]) -> Result<(), FileError> {
		write(root, path, None, false, data)
	}

	#[derive(Serialize)]
	struct Header<'a> {
		root: &'a str,
		path: &'a str,
		#[serde(skip_serializing_if = "Option::is_none")]
		offset: Option<u64>,
		#[serde(skip_serializing_if = "std::ops::Not::not")]
		truncate: bool,
	}

	fn decode<T: serde::de::DeserializeOwned>(bytes: Vec<u8>) -> Result<T, FileError> {
		serde_json::from_slice(&bytes).map_err(|_| FileError::Reply)
	}

	fn request(value: &serde_json::Value) -> Result<Vec<u8>, FileError> {
		let bytes = serde_json::to_vec(value).map_err(|_| FileError::Reply)?;
		if bytes.len() > 4096 {
			return Err(FileError::Host(HostError::Invalid));
		}
		call(&bytes)
	}

	#[cfg(target_arch = "wasm32")]
	fn reply(code: i32) -> Result<Vec<u8>, FileError> {
		let (capacity, failed) = match code {
			0.. => (code as usize, false),
			-3 => (1024, true),
			-1 => return Err(FileError::Host(HostError::Invalid)),
			_ => return Err(FileError::Host(HostError::Unavailable)),
		};
		let mut reply = vec![0_u8; capacity];
		// SAFETY: the host writes at most `capacity` bytes into this exclusive buffer.
		let read = unsafe { super::raw::result_read(reply.as_mut_ptr(), capacity as i32) };
		if read < 0 {
			return Err(FileError::Reply);
		}
		reply.truncate(read as usize);
		if failed {
			Err(FileError::Failed(
				String::from_utf8_lossy(&reply).into_owned(),
			))
		} else {
			Ok(reply)
		}
	}

	#[cfg(target_arch = "wasm32")]
	fn call(request: &[u8]) -> Result<Vec<u8>, FileError> {
		// SAFETY: the host reads `request.len()` bytes from this live buffer.
		reply(unsafe { super::raw::file_request(request.as_ptr(), request.len() as i32) })
	}

	#[cfg(target_arch = "wasm32")]
	fn call_write(header: &[u8], data: &[u8]) -> Result<(), FileError> {
		// SAFETY: the host reads both live buffers, `header.len()` and `data.len()` bytes.
		let code = unsafe {
			super::raw::file_write(
				header.as_ptr(),
				header.len() as i32,
				data.as_ptr(),
				data.len() as i32,
			)
		};
		reply(code).map(drop)
	}

	#[cfg(not(target_arch = "wasm32"))]
	fn call(_: &[u8]) -> Result<Vec<u8>, FileError> {
		Err(FileError::Host(HostError::Unavailable))
	}

	#[cfg(not(target_arch = "wasm32"))]
	fn call_write(_: &[u8], _: &[u8]) -> Result<(), FileError> {
		Err(FileError::Host(HostError::Unavailable))
	}
}
