//! Files: a private folder per plugin, and folders or files the user picked for it. A plugin
//! names a root and a relative path in each request; the host resolves it, refuses anything
//! that leaves the root, and never hands the plugin an absolute path.
use crate::Error;
use serde::{Deserialize, Serialize};

/// Largest `file_request` request, in bytes.
pub const MAX_FILE_REQUEST_BYTES: u32 = 4096;
/// Largest data one `file_write` call writes.
pub const MAX_FILE_WRITE_BYTES: u32 = 1024 * 1024;
/// Largest data one `read` request returns.
pub const MAX_FILE_READ_BYTES: u32 = 256 * 1024;
/// Most entries one `list` request returns.
pub const MAX_FILE_LIST_ENTRIES: u32 = 512;
/// Most folders and files the user may give one plugin.
pub const MAX_FILE_HANDLES: usize = 16;
/// Longest relative path, in UTF-8 bytes.
pub const MAX_FILE_PATH_BYTES: usize = 1024;
/// The root naming the plugin's own folder.
pub const PRIVATE_ROOT: &str = "private";

/// One `file_request`, as JSON tagged by `op`. `root` is [`PRIVATE_ROOT`] or the id of a
/// folder or file the user picked; `path` is relative to it, `/`-separated, and empty for
/// the root itself.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileRequest {
	/// The folders and files the user gave this plugin.
	Handles,
	/// Up to `limit` entries of a folder after the first `offset`, sorted by name.
	List {
		root: String,
		#[serde(default)]
		path: String,
		#[serde(default)]
		offset: u32,
		#[serde(default = "list_limit")]
		limit: u32,
	},
	Stat {
		root: String,
		#[serde(default)]
		path: String,
	},
	/// Raw bytes from `offset`, at most `length`; fewer at the end of the file.
	Read {
		root: String,
		#[serde(default)]
		path: String,
		#[serde(default)]
		offset: u64,
		length: u32,
	},
	/// Creates a folder and its missing parents.
	CreateFolder { root: String, path: String },
	/// Removes a file or an empty folder.
	Delete { root: String, path: String },
	/// Moves within the same root, never over an existing entry.
	Rename {
		root: String,
		path: String,
		to: String,
	},
	/// What became of this plugin's recent `attachment_fetch` requests. Requires
	/// `media_archive`.
	Fetches,
}

/// Largest attachment one `attachment_fetch` saves.
pub const MAX_ARCHIVE_FILE_BYTES: u64 = 100 * 1024 * 1024;
/// Fetches one plugin may have waiting.
pub const MAX_QUEUED_FETCHES: usize = 256;

/// One `attachment_fetch`: save the Discord attachment at `url`, of `size` bytes, to `path`
/// in the plugin's private folder. The download runs after the invocation; the plugin
/// learns the outcome from the `fetches` file request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentFetch {
	pub url: String,
	pub size: u64,
	pub path: String,
}

fn list_limit() -> u32 {
	MAX_FILE_LIST_ENTRIES
}

/// The header of one `file_write`; the data travels separately as raw bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileWrite {
	pub root: String,
	#[serde(default)]
	pub path: String,
	/// Where to write; omitted appends.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub offset: Option<u64>,
	/// Empty the file first.
	#[serde(default, skip_serializing_if = "crate::interface_ui::is_false")]
	pub truncate: bool,
}

/// What the user is asked to pick.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickKind {
	Folder,
	/// An existing file.
	File,
	/// A new or existing file to write, such as an export.
	SaveFile,
}

/// Asks the user to pick a folder or file for this plugin. On a pick the host runs `action`,
/// a declared panel action, with the new root id in `values["root"]`; on cancel it runs it
/// with `values["cancelled"] = "true"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilePick {
	pub kind: PickKind,
	/// Whether the plugin may change what is picked; a `save_file` always may.
	#[serde(default, skip_serializing_if = "crate::interface_ui::is_false")]
	pub write: bool,
	/// Why, shown in the picker title and the plugin's details. At most 128 bytes.
	pub purpose: String,
	/// The file name a `save_file` picker suggests.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	pub action: String,
}

impl FilePick {
	pub fn validate(&self, panel_action: impl Fn(&str) -> bool) -> Result<(), Error> {
		let line = |text: &str| {
			!text.trim().is_empty() && text.len() <= 128 && !text.chars().any(char::is_control)
		};
		if !line(&self.purpose)
			|| self
				.name
				.as_deref()
				.is_some_and(|name| !line(name) || name.contains(['/', '\\']))
		{
			return Err(Error::Invalid);
		}
		if self.name.is_some() && self.kind != PickKind::SaveFile {
			return Err(Error::Invalid);
		}
		if !panel_action(&self.action) {
			return Err(Error::Capability);
		}
		Ok(())
	}
}

/// A folder or file the user gave a plugin, as the plugin sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHandleInfo {
	pub root: String,
	pub kind: PickKind,
	pub write: bool,
	/// The picked folder's or file's own name, never its full path.
	pub name: String,
	pub purpose: String,
}

/// Splits a relative path into its parts, refusing anything that could leave the root:
/// absolute paths, `..`, `.`, empty parts, backslashes, drive or stream syntax, and names
/// Windows reserves.
pub fn path_parts(path: &str) -> Result<Vec<&str>, Error> {
	if path.len() > MAX_FILE_PATH_BYTES {
		return Err(Error::Limit);
	}
	if path.is_empty() {
		return Ok(Vec::new());
	}
	let mut parts = Vec::new();
	for part in path.split('/') {
		let reserved = {
			let stem = part
				.split('.')
				.next()
				.unwrap_or_default()
				.to_ascii_uppercase();
			matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
				|| (stem.len() == 4
					&& (stem.starts_with("COM") || stem.starts_with("LPT"))
					&& stem.as_bytes()[3].is_ascii_digit())
		};
		if part.is_empty()
			|| part == "."
			|| part == ".."
			|| part.len() > 255
			|| part.ends_with(['.', ' '])
			|| part.chars().any(|c| {
				c.is_control() || matches!(c, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
			}) || reserved
		{
			return Err(Error::Invalid);
		}
		parts.push(part);
	}
	Ok(parts)
}
