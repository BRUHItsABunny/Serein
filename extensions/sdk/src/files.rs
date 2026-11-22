//! Asking the user for a folder or file (preview); read and write it with
//! [`crate::host::files`].
use crate::Output;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PickKind {
	Folder,
	/// An existing file.
	File,
	/// A new or existing file to write, such as an export.
	SaveFile,
}

/// Asks the user to pick a folder or file. Afterwards the host runs `action`, a declared
/// panel action, with the new root id in `values["root"]`, or with
/// `values["cancelled"] = "true"`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePick {
	pub kind: PickKind,
	/// Whether you may change what is picked; a `save_file` always may.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub write: bool,
	/// Why, shown in the picker title and your details page. At most 128 bytes.
	pub purpose: String,
	/// The file name a `save_file` picker suggests.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub name: Option<String>,
	pub action: String,
}

/// A folder or file the user gave you, from [`crate::host::files::handles`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHandleInfo {
	/// The root id to use in file calls.
	pub root: String,
	pub kind: PickKind,
	pub write: bool,
	/// Its own name, never its full path.
	pub name: String,
	pub purpose: String,
}

/// Output of an activation or panel action asking for a pick.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickOutput {
	#[serde(flatten)]
	pub output: Output,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pick: Option<FilePick>,
}
