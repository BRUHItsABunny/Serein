//! Capability-gated host functions a plugin module may import from the `serein` module.
//!
//! Every import names the capability it requires. A package whose module imports anything
//! else, or imports a function without declaring its capability, fails validation; an
//! invocation whose grants lack an imported function's capability fails before any Wasm runs.
//! Each call costs fuel and counts against a per-invocation call budget. Host functions never
//! hand Wasm a host pointer: data crosses only as bytes copied into the module's own memory.
//!
//! Functions whose reply has a variable size, such as `db_execute`, return its byte length
//! and keep the reply pending; `result_read` then copies it into a buffer the module sized.
use crate::Capability;

/// Wasm import module name for every host function.
pub const IMPORT_MODULE: &str = "serein";
/// Host calls one invocation may make, across every import.
pub const MAX_HOST_CALLS: u32 = 4096;
/// Fuel charged per host call, on top of per-byte costs.
pub(crate) const CALL_FUEL: u64 = 1_000;
/// Fuel charged per byte a host call copies into or out of Wasm memory.
pub(crate) const BYTE_FUEL: u64 = 4;
/// Largest buffer one `random_fill` call may fill.
pub const MAX_RANDOM_BYTES: u32 = 4096;
/// Largest request one `db_execute` call may send.
pub const MAX_DB_REQUEST_BYTES: u32 = 256 * 1024;
/// Largest pending reply; a larger successful result is replaced by an error.
pub const MAX_REPLY_BYTES: usize = 256 * 1024;
/// Largest error message a failed call leaves pending.
pub const MAX_ERROR_BYTES: usize = 1024;
/// Largest request one `https_request` call may describe.
pub const MAX_HTTPS_REQUEST_BYTES: u32 = 256 * 1024;
/// Most entries one `journal_read` call returns.
pub const MAX_JOURNAL_ENTRIES: u32 = 256;

/// Status codes host functions return as `i32`. Zero or more is success.
pub mod status {
	pub const OK: i32 = 0;
	/// Arguments are out of range, such as an oversized length or a missing reply.
	pub const INVALID: i32 = -1;
	/// The host cannot provide the service right now.
	pub const UNAVAILABLE: i32 = -2;
	/// The service rejected the request; a UTF-8 message of at most
	/// [`MAX_ERROR_BYTES`](super::MAX_ERROR_BYTES) is pending for `result_read`.
	pub const FAILED: i32 = -3;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
	/// `() -> i64`: Unix time in milliseconds, or `-1` when unavailable.
	ClockNowMs,
	/// `(ptr: i32, len: i32) -> i32`: fills Wasm memory with random bytes; a [`status`] code.
	RandomFill,
	/// `(ptr: i32, len: i32) -> i32`: runs one JSON database request; the pending reply's
	/// length, or a [`status`] code.
	DbExecute,
	/// `(ptr: i32, cap: i32) -> i32`: copies the pending reply when it fits in `cap` bytes and
	/// clears it; the copied length, or [`status::INVALID`].
	ResultRead,
	/// `(max: i32) -> i32`: reads up to `max` journal entries after the plugin's cursor; the
	/// pending reply's length, or a [`status`] code.
	JournalRead,
	/// `(ptr: i32, len: i32) -> i32`: performs one JSON-described HTTPS request to a declared
	/// origin; the pending response's length, or a [`status`] code.
	HttpsRequest,
	/// `(ptr: i32, len: i32) -> i32`: broadcasts `{"topic", "payload"}`; how many approved
	/// subscribers received it, or a [`status`] code.
	Broadcast,
	/// `(ptr: i32, len: i32) -> i32`: whether a topic name has an approved subscriber (`1`)
	/// or not (`0`), or a [`status`] code.
	HasSubscribers,
	/// `(max: i32) -> i32`: reads up to `max` messages from approved providers; the pending
	/// reply's length, or a [`status`] code.
	TopicRead,
	/// `(ptr: i32, len: i32) -> i32`: runs one JSON [`crate::FileRequest`]; the pending
	/// reply's length, or a [`status`] code.
	FileRequest,
	/// `(ptr: i32, len: i32, data: i32, data_len: i32) -> i32`: writes raw bytes as a JSON
	/// [`crate::FileWrite`] header says; [`status::OK`] or a [`status`] code.
	FileWrite,
}

/// One importable host function.
#[derive(Clone, Copy, Debug)]
pub struct HostImport {
	pub name: &'static str,
	/// Capability the manifest must declare and the user must grant; `None` for functions
	/// that only return replies other gated calls produced.
	pub capability: Option<Capability>,
	pub(crate) kind: Kind,
}

/// Every host function this revision provides, in a stable order.
pub const HOST_IMPORTS: &[HostImport] = &[
	HostImport {
		name: "clock_now_ms",
		capability: Some(Capability::Clock),
		kind: Kind::ClockNowMs,
	},
	HostImport {
		name: "random_fill",
		capability: Some(Capability::Random),
		kind: Kind::RandomFill,
	},
	HostImport {
		name: "db_execute",
		capability: Some(Capability::Database),
		kind: Kind::DbExecute,
	},
	HostImport {
		name: "result_read",
		capability: None,
		kind: Kind::ResultRead,
	},
	HostImport {
		name: "journal_read",
		capability: Some(Capability::MessageJournal),
		kind: Kind::JournalRead,
	},
	HostImport {
		name: "https_request",
		capability: Some(Capability::Network),
		kind: Kind::HttpsRequest,
	},
	// Topic functions are checked against the manifest's topic lists at each call.
	HostImport {
		name: "broadcast",
		capability: None,
		kind: Kind::Broadcast,
	},
	HostImport {
		name: "has_subscribers",
		capability: None,
		kind: Kind::HasSubscribers,
	},
	HostImport {
		name: "topic_read",
		capability: None,
		kind: Kind::TopicRead,
	},
	HostImport {
		name: "file_request",
		capability: Some(Capability::Files),
		kind: Kind::FileRequest,
	},
	HostImport {
		name: "file_write",
		capability: Some(Capability::Files),
		kind: Kind::FileWrite,
	},
];

pub(crate) fn find(name: &str) -> Option<&'static HostImport> {
	HOST_IMPORTS.iter().find(|import| import.name == name)
}

/// Answer to a request with a variable-size reply.
#[derive(Debug, PartialEq, Eq)]
pub enum Reply {
	/// The reply bytes.
	Ok(Vec<u8>),
	/// The request was rejected; a short UTF-8 message for the plugin.
	Failed(String),
	/// The service is not available to this invocation.
	Unavailable,
}

/// Services behind the host functions. The front end supplies one per invocation; it runs
/// on the extension worker thread, never the UI thread.
pub trait HostImports: Send {
	/// Current Unix time in milliseconds.
	fn now_ms(&mut self) -> Option<i64>;
	/// Fills `out` with cryptographically secure random bytes.
	fn fill_random(&mut self, out: &mut [u8]) -> bool;
	/// Runs one database request, `{"sql": "...", "params": [...]}` as JSON, against the
	/// plugin's own database.
	fn db_execute(&mut self, _request: &[u8]) -> Reply {
		Reply::Unavailable
	}
	/// Reads up to `max_entries` message journal entries after the plugin's cursor, as
	/// `{"entries": [...], "more": bool}` JSON. The cursor advances only when the host
	/// accepts the invocation.
	fn journal_read(&mut self, _max_entries: u32) -> Reply {
		Reply::Unavailable
	}
	/// Performs one HTTPS request described by JSON to an origin the manifest declares, and
	/// replies with the response as JSON. Blocks this invocation until it completes or the
	/// host's time limit ends it.
	fn https_request(&mut self, _request: &[u8]) -> Reply {
		Reply::Unavailable
	}
	/// Broadcasts `{"topic": "...", "payload": ...}` JSON on a topic the manifest declares in
	/// `broadcasts`; returns the number of approved subscribers that received it, or a
	/// negative [`status`] code.
	fn broadcast(&mut self, _request: &[u8]) -> i32 {
		status::UNAVAILABLE
	}
	/// Whether `topic` has an approved subscriber: `1` or `0`, or a negative [`status`].
	fn has_subscribers(&mut self, _topic: &str) -> i32 {
		status::UNAVAILABLE
	}
	/// Reads up to `max` messages delivered to this plugin, as
	/// `{"messages": [...], "more": bool}` JSON. They leave its queue only when the host
	/// accepts the invocation.
	fn topic_read(&mut self, _max: u32) -> Reply {
		Reply::Unavailable
	}
	/// Runs one [`crate::FileRequest`] in the plugin's private folder or a folder or file the
	/// user gave it. `read` replies with raw bytes, the others with JSON.
	fn file_request(&mut self, _request: &[u8]) -> Reply {
		Reply::Unavailable
	}
	/// Writes `data` as the [`crate::FileWrite`] header says; an empty reply on success.
	fn file_write(&mut self, _header: &[u8], _data: &[u8]) -> Reply {
		Reply::Unavailable
	}
}

/// Provides no services: every call reports [`status::UNAVAILABLE`].
pub struct NoHostImports;
impl HostImports for NoHostImports {
	fn now_ms(&mut self) -> Option<i64> {
		None
	}
	fn fill_random(&mut self, _: &mut [u8]) -> bool {
		false
	}
}
