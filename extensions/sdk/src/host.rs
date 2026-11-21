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
