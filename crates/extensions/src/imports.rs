//! Capability-gated host functions a plugin module may import from the `serein` module.
//!
//! Every import names the capability it requires. A package whose module imports anything
//! else, or imports a function without declaring its capability, fails validation; an
//! invocation whose grants lack an imported function's capability fails before any Wasm runs.
//! Each call costs fuel and counts against a per-invocation call budget. Host functions never
//! hand Wasm a host pointer: data crosses only as bytes copied into the module's own memory.
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

/// Status codes host functions return as `i32`. Zero is success.
pub mod status {
	pub const OK: i32 = 0;
	/// Arguments are out of range, such as an oversized length.
	pub const INVALID: i32 = -1;
	/// The host cannot provide the service right now.
	pub const UNAVAILABLE: i32 = -2;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
	/// `() -> i64`: Unix time in milliseconds, or `-1` when unavailable.
	ClockNowMs,
	/// `(ptr: i32, len: i32) -> i32`: fills Wasm memory with random bytes; a [`status`] code.
	RandomFill,
}

/// One importable host function.
#[derive(Clone, Copy, Debug)]
pub struct HostImport {
	pub name: &'static str,
	pub capability: Capability,
	pub(crate) kind: Kind,
}

/// Every host function this revision provides, in a stable order.
pub const HOST_IMPORTS: &[HostImport] = &[
	HostImport {
		name: "clock_now_ms",
		capability: Capability::Clock,
		kind: Kind::ClockNowMs,
	},
	HostImport {
		name: "random_fill",
		capability: Capability::Random,
		kind: Kind::RandomFill,
	},
];

pub(crate) fn find(name: &str) -> Option<&'static HostImport> {
	HOST_IMPORTS.iter().find(|import| import.name == name)
}

/// Services behind the host functions. The front end supplies one per invocation; it runs
/// on the extension worker thread, never the UI thread.
pub trait HostImports: Send {
	/// Current Unix time in milliseconds.
	fn now_ms(&mut self) -> Option<i64>;
	/// Fills `out` with cryptographically secure random bytes.
	fn fill_random(&mut self, out: &mut [u8]) -> bool;
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
