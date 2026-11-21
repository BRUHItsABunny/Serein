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
