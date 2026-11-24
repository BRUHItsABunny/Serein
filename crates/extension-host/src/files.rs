//! Files a plugin may use: its private folder under its data directory, bounded by its data
//! limit, and folders or files the user picked for it. Every request names a root and a
//! relative path; resolution refuses `..`, absolute paths and symbolic links anywhere under
//! the root, so a plugin cannot reach outside what it was given. Changes apply at once,
//! not only when the invocation succeeds.
use extensions::{
	FileHandleInfo, FileRequest, FileWrite, PickKind, Reply,
	files::{
		MAX_FILE_HANDLES, MAX_FILE_LIST_ENTRIES, MAX_FILE_READ_BYTES, PRIVATE_ROOT, path_parts,
	},
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The private folder's name inside the plugin's data directory.
pub const PRIVATE_FOLDER: &str = "files";
/// Entries scanned for a listing or the private folder's size; larger trees are refused.
const MAX_SCAN_ENTRIES: usize = 100_000;
/// Bytes one invocation may write, and read, across all its calls.
const MAX_INVOCATION_BYTES: u64 = 512 * 1024 * 1024;

/// A folder or file the user picked for a plugin, saved with the plugin's data settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileHandle {
	/// The root id the plugin uses.
	pub id: String,
	pub kind: PickKind,
	pub write: bool,
	pub path: PathBuf,
	pub purpose: String,
}

impl FileHandle {
	pub fn info(&self) -> FileHandleInfo {
		FileHandleInfo {
			root: self.id.clone(),
			kind: self.kind,
			write: self.write,
			name: self
				.path
				.file_name()
				.map(|name| name.to_string_lossy().into_owned())
				.unwrap_or_default(),
			purpose: self.purpose.clone(),
		}
	}
}

/// A new root id: `h` and 16 hex digits.
pub fn new_handle_id(existing: &[FileHandle]) -> Option<String> {
	if existing.len() >= MAX_FILE_HANDLES {
		return None;
	}
	loop {
		let mut bytes = [0u8; 8];
		getrandom::fill(&mut bytes).ok()?;
		let id = format!("h{:016x}", u64::from_le_bytes(bytes));
		if !existing.iter().any(|handle| handle.id == id) {
			return Some(id);
		}
	}
}

/// The private folder's size, or `None` when it holds more than can be scanned.
pub fn folder_bytes(folder: &Path) -> Option<u64> {
	let mut total = 0u64;
	let mut scanned = 0;
	let mut pending = vec![folder.to_path_buf()];
	while let Some(folder) = pending.pop() {
		let Ok(entries) = fs::read_dir(&folder) else {
			continue;
		};
		for entry in entries.flatten() {
			scanned += 1;
			if scanned > MAX_SCAN_ENTRIES {
				return None;
			}
			let Ok(metadata) = entry.metadata() else {
				continue;
			};
			if metadata.is_dir() {
				pending.push(entry.path());
			} else if metadata.is_file() {
				total = total.saturating_add(metadata.len());
			}
		}
	}
	Some(total)
}

/// One invocation's file access.
pub struct PluginFiles {
	private: PathBuf,
	quota: u64,
	handles: Vec<FileHandle>,
	/// The private folder's size, measured on the first write.
	used: Option<u64>,
	written: u64,
	read: u64,
}

struct Resolved {
	target: PathBuf,
	private: bool,
	/// A picked file: only stat, read and write apply.
	file: bool,
}

fn failed(message: &str) -> Reply {
	Reply::Failed(message.into())
}

fn modified_ms(metadata: &fs::Metadata) -> Option<i64> {
	let since = metadata
		.modified()
		.ok()?
		.duration_since(std::time::UNIX_EPOCH)
		.ok()?;
	i64::try_from(since.as_millis()).ok()
}

impl PluginFiles {
	pub fn new(data: &Path, quota: u64, handles: Vec<FileHandle>) -> Self {
		Self {
			private: data.join(PRIVATE_FOLDER),
			quota,
			handles,
			used: None,
			written: 0,
			read: 0,
		}
	}

	/// Resolves `root` and `path`, checking the handle allows `write` and that no existing
	/// part of the path is a symbolic link.
	fn resolve(&self, root: &str, path: &str, write: bool) -> Result<Resolved, &'static str> {
		let parts = path_parts(path).map_err(|_| "Invalid path")?;
		let (base, private, file) = if root == PRIVATE_ROOT {
			(self.private.clone(), true, false)
		} else {
			let handle = self
				.handles
				.iter()
				.find(|handle| handle.id == root)
				.ok_or("Unknown root")?;
			if write && !handle.write {
				return Err("This folder or file is read-only for this plugin");
			}
			let file = handle.kind != PickKind::Folder;
			if file && !parts.is_empty() {
				return Err("A picked file has no paths inside it");
			}
			(handle.path.clone(), false, file)
		};
		let mut target = base.clone();
		let check = |path: &Path| match fs::symlink_metadata(path) {
			Ok(metadata) if metadata.file_type().is_symlink() => {
				Err("Symbolic links are not followed")
			}
			_ => Ok(()),
		};
		check(&base)?;
		for part in parts {
			target.push(part);
			check(&target)?;
		}
		Ok(Resolved {
			target,
			private,
			file,
		})
	}

	/// Where a new file at `path` in the private folder goes, checked like a write.
	pub(crate) fn private_target(&self, path: &str) -> Result<PathBuf, &'static str> {
		if path.is_empty() {
			return Err("Name a file inside the folder");
		}
		Ok(self.resolve(PRIVATE_ROOT, path, true)?.target)
	}
	fn handles(&self) -> Reply {
		let handles: Vec<_> = self.handles.iter().map(FileHandle::info).collect();
		json(&serde_json::json!({ "handles": handles }))
	}

	pub fn request(&mut self, request: &[u8]) -> Reply {
		let Ok(request) = serde_json::from_slice::<FileRequest>(request) else {
			return failed("Invalid file request");
		};
		match self.run(request) {
			Ok(reply) => reply,
			Err(message) => failed(message),
		}
	}

	fn run(&mut self, request: FileRequest) -> Result<Reply, &'static str> {
		Ok(match request {
			FileRequest::Handles => self.handles(),
			FileRequest::Fetches => return Err("Attachment fetches need media_archive"),
			FileRequest::Stat { root, path } => {
				let resolved = self.resolve(&root, &path, false)?;
				match fs::metadata(&resolved.target) {
					Ok(metadata) => json(&serde_json::json!({
						"exists": true,
						"folder": metadata.is_dir(),
						"size": metadata.len(),
						"modified_ms": modified_ms(&metadata),
					})),
					Err(_) => json(&serde_json::json!({ "exists": false })),
				}
			}
			FileRequest::List {
				root,
				path,
				offset,
				limit,
			} => {
				let resolved = self.resolve(&root, &path, false)?;
				if resolved.file {
					return Err("A picked file is not a folder");
				}
				let mut names = Vec::new();
				let entries = fs::read_dir(&resolved.target).map_err(|_| "Not a folder")?;
				for entry in entries.flatten() {
					if names.len() >= MAX_SCAN_ENTRIES {
						return Err("The folder holds too many entries to list");
					}
					names.push(entry);
				}
				names.sort_by_key(|entry| entry.file_name());
				let limit = limit.clamp(1, MAX_FILE_LIST_ENTRIES) as usize;
				let total = names.len();
				let entries: Vec<_> = names
					.into_iter()
					.skip(offset as usize)
					.take(limit)
					.filter_map(|entry| {
						let name = entry.file_name().into_string().ok()?;
						let metadata = fs::symlink_metadata(entry.path()).ok()?;
						// Links are left out, as they are never followed.
						if metadata.file_type().is_symlink() {
							return None;
						}
						Some(serde_json::json!({
							"name": name,
							"folder": metadata.is_dir(),
							"size": metadata.len(),
							"modified_ms": modified_ms(&metadata),
						}))
					})
					.collect();
				json(&serde_json::json!({
					"entries": entries,
					"more": total > offset as usize + limit,
				}))
			}
			FileRequest::Read {
				root,
				path,
				offset,
				length,
			} => {
				let resolved = self.resolve(&root, &path, false)?;
				let length = length.min(MAX_FILE_READ_BYTES);
				if self.read.saturating_add(u64::from(length)) > MAX_INVOCATION_BYTES {
					return Err("This invocation read too much; continue in the next one");
				}
				let mut file =
					fs::File::open(&resolved.target).map_err(|_| "Cannot open the file")?;
				file.seek(SeekFrom::Start(offset))
					.map_err(|_| "Cannot read the file")?;
				let mut bytes = Vec::with_capacity(length as usize);
				file.take(u64::from(length))
					.read_to_end(&mut bytes)
					.map_err(|_| "Cannot read the file")?;
				self.read += bytes.len() as u64;
				Reply::Ok(bytes)
			}
			FileRequest::CreateFolder { root, path } => {
				let resolved = self.resolve(&root, &path, true)?;
				if resolved.file || path.is_empty() {
					return Err("Invalid path");
				}
				fs::create_dir_all(&resolved.target).map_err(|_| "Cannot create the folder")?;
				Reply::Ok(Vec::new())
			}
			FileRequest::Delete { root, path } => {
				let resolved = self.resolve(&root, &path, true)?;
				if resolved.file || path.is_empty() {
					return Err("The root itself cannot be deleted");
				}
				let metadata = fs::symlink_metadata(&resolved.target).map_err(|_| "Not found")?;
				if metadata.is_dir() {
					fs::remove_dir(&resolved.target)
						.map_err(|_| "Only empty folders can be deleted")?;
				} else {
					fs::remove_file(&resolved.target).map_err(|_| "Cannot delete the file")?;
					if resolved.private
						&& let Some(used) = &mut self.used
					{
						*used = used.saturating_sub(metadata.len());
					}
				}
				Reply::Ok(Vec::new())
			}
			FileRequest::Rename { root, path, to } => {
				let from = self.resolve(&root, &path, true)?;
				let to = self.resolve(&root, &to, true)?;
				if from.file || path.is_empty() {
					return Err("Invalid path");
				}
				if fs::symlink_metadata(&to.target).is_ok() {
					return Err("Something already has that name");
				}
				fs::rename(&from.target, &to.target).map_err(|_| "Cannot rename")?;
				Reply::Ok(Vec::new())
			}
		})
	}

	pub fn write(&mut self, header: &[u8], data: &[u8]) -> Reply {
		let Ok(header) = serde_json::from_slice::<FileWrite>(header) else {
			return failed("Invalid file write");
		};
		match self.write_inner(header, data) {
			Ok(()) => Reply::Ok(Vec::new()),
			Err(message) => failed(message),
		}
	}

	fn write_inner(&mut self, header: FileWrite, data: &[u8]) -> Result<(), &'static str> {
		let resolved = self.resolve(&header.root, &header.path, true)?;
		if header.path.is_empty() && !resolved.file {
			return Err("Name a file inside the folder");
		}
		if self.written.saturating_add(data.len() as u64) > MAX_INVOCATION_BYTES {
			return Err("This invocation wrote too much; continue in the next one");
		}
		let old = fs::metadata(&resolved.target).map_or(0, |metadata| metadata.len());
		let start = if header.truncate {
			header.offset.unwrap_or(0)
		} else {
			header.offset.unwrap_or(old)
		};
		let end = start.saturating_add(data.len() as u64);
		let new = if header.truncate { end } else { old.max(end) };
		if resolved.private {
			if self.used.is_none() {
				fs::create_dir_all(&self.private).map_err(|_| "Cannot create the folder")?;
				self.used =
					Some(folder_bytes(&self.private).ok_or("The folder holds too many files")?);
			}
			let used = self.used.unwrap_or_default();
			if used.saturating_sub(old).saturating_add(new) > self.quota {
				return Err("The plugin's data limit is reached");
			}
		}
		let mut file = fs::OpenOptions::new()
			.create(true)
			.write(true)
			.truncate(header.truncate)
			.open(&resolved.target)
			.map_err(|_| "Cannot open the file for writing")?;
		file.seek(SeekFrom::Start(start))
			.and_then(|_| file.write_all(data))
			.map_err(|_| "Cannot write the file")?;
		self.written += data.len() as u64;
		if resolved.private
			&& let Some(used) = &mut self.used
		{
			*used = used.saturating_sub(old).saturating_add(new);
		}
		Ok(())
	}
}

fn json(value: &serde_json::Value) -> Reply {
	serde_json::to_vec(value).map_or_else(|_| failed("Cannot encode the reply"), Reply::Ok)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn temp(name: &str) -> PathBuf {
		let mut nonce = [0u8; 8];
		getrandom::fill(&mut nonce).unwrap();
		let path = std::env::temp_dir().join(format!(
			"serein-files-{name}-{:016x}",
			u64::from_le_bytes(nonce)
		));
		fs::create_dir_all(&path).unwrap();
		path
	}
	fn request(files: &mut PluginFiles, value: serde_json::Value) -> Reply {
		files.request(&serde_json::to_vec(&value).unwrap())
	}
	fn write(files: &mut PluginFiles, value: serde_json::Value, data: &[u8]) -> Reply {
		files.write(&serde_json::to_vec(&value).unwrap(), data)
	}

	/// Paths that could leave a root are refused before touching the disk, a read-only
	/// pick refuses changes, a picked file has nothing inside it, and only handles the user
	/// gave resolve.
	#[test]
	fn requests_stay_inside_their_root_and_respect_read_only_picks() {
		let data = temp("data");
		let outside = temp("outside");
		fs::write(outside.join("secret.txt"), b"keep out").unwrap();
		let picked = temp("picked");
		fs::write(picked.join("notes.txt"), b"hello").unwrap();
		let mut files = PluginFiles::new(
			&data,
			1024,
			vec![
				FileHandle {
					id: "hfolder".into(),
					kind: PickKind::Folder,
					write: false,
					path: picked.clone(),
					purpose: "Import".into(),
				},
				FileHandle {
					id: "hfile".into(),
					kind: PickKind::SaveFile,
					write: true,
					path: outside.join("export.txt"),
					purpose: "Export".into(),
				},
			],
		);
		for path in [
			"../outside/secret.txt",
			"..",
			"a/../../x",
			"/etc/passwd",
			"C:/Windows",
			r"a\..\b",
			"file.txt:stream",
			"NUL",
			"com1.txt",
			"a//b",
			"trailing.",
		] {
			assert!(
				matches!(
					request(
						&mut files,
						serde_json::json!({"op": "stat", "root": "private", "path": path})
					),
					Reply::Failed(_)
				),
				"{path}"
			);
		}
		assert!(matches!(
			request(
				&mut files,
				serde_json::json!({"op": "stat", "root": "hunknown"})
			),
			Reply::Failed(_)
		));
		// A read-only folder reads but does not change.
		assert_eq!(
			request(
				&mut files,
				serde_json::json!({"op": "read", "root": "hfolder", "path": "notes.txt", "offset": 1, "length": 3})
			),
			Reply::Ok(b"ell".to_vec())
		);
		assert!(matches!(
			write(
				&mut files,
				serde_json::json!({"root": "hfolder", "path": "notes.txt"}),
				b"x"
			),
			Reply::Failed(_)
		));
		assert!(matches!(
			request(
				&mut files,
				serde_json::json!({"op": "delete", "root": "hfolder", "path": "notes.txt"})
			),
			Reply::Failed(_)
		));
		// A picked file is written as itself, and nothing inside it resolves.
		assert_eq!(
			write(
				&mut files,
				serde_json::json!({"root": "hfile", "truncate": true}),
				b"export"
			),
			Reply::Ok(Vec::new())
		);
		assert_eq!(fs::read(outside.join("export.txt")).unwrap(), b"export");
		assert!(matches!(
			request(
				&mut files,
				serde_json::json!({"op": "stat", "root": "hfile", "path": "x"})
			),
			Reply::Failed(_)
		));
		assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"keep out");
		let Reply::Ok(handles) = request(&mut files, serde_json::json!({"op": "handles"})) else {
			panic!("handles")
		};
		let handles: serde_json::Value = serde_json::from_slice(&handles).unwrap();
		assert_eq!(
			handles["handles"][1]["name"], "export.txt",
			"names, never full paths"
		);
		assert_eq!(
			handles["handles"][0],
			serde_json::json!({
				"root": "hfolder",
				"kind": "folder",
				"write": false,
				"name": picked.file_name().unwrap().to_string_lossy(),
				"purpose": "Import",
			})
		);
		for path in [data, outside, picked] {
			let _ = fs::remove_dir_all(path);
		}
	}

	/// The private folder's writes count against the data limit across offsets, appends,
	/// truncation and deletes; listing pages through sorted entries.
	#[test]
	fn private_writes_stay_within_the_data_limit_and_listings_page() {
		let data = temp("quota");
		let mut files = PluginFiles::new(&data, 10, Vec::new());
		let private = |path: &str| serde_json::json!({"root": "private", "path": path});
		assert_eq!(
			write(&mut files, private("a.bin"), b"12345678"),
			Reply::Ok(Vec::new())
		);
		assert!(
			matches!(
				write(&mut files, private("b.bin"), b"123"),
				Reply::Failed(_)
			),
			"8 + 3 bytes exceed 10"
		);
		// Overwriting in place does not grow the file; appending does.
		let mut at = private("a.bin");
		at["offset"] = 0.into();
		assert_eq!(write(&mut files, at, b"abcdefgh"), Reply::Ok(Vec::new()));
		assert_eq!(
			write(&mut files, private("a.bin"), b"ij"),
			Reply::Ok(Vec::new())
		);
		assert!(matches!(
			write(&mut files, private("a.bin"), b"k"),
			Reply::Failed(_)
		));
		let mut truncate = private("a.bin");
		truncate["truncate"] = true.into();
		assert_eq!(write(&mut files, truncate, b"1"), Reply::Ok(Vec::new()));
		assert_eq!(
			fs::read(data.join(PRIVATE_FOLDER).join("a.bin")).unwrap(),
			b"1"
		);
		assert_eq!(
			request(
				&mut files,
				serde_json::json!({"op": "delete", "root": "private", "path": "a.bin"})
			),
			Reply::Ok(Vec::new())
		);
		assert_eq!(
			write(&mut files, private("b.bin"), b"0123456789"),
			Reply::Ok(Vec::new())
		);

		// A fresh invocation measures the folder again.
		let mut files = PluginFiles::new(&data, 10, Vec::new());
		assert!(matches!(
			write(&mut files, private("c.bin"), b"x"),
			Reply::Failed(_)
		));
		let mut files = PluginFiles::new(&data, 1024, Vec::new());
		assert_eq!(
			request(
				&mut files,
				serde_json::json!({"op": "create_folder", "root": "private", "path": "logs/2026"})
			),
			Reply::Ok(Vec::new())
		);
		for name in ["c", "a", "b"] {
			write(&mut files, private(&format!("logs/{name}.txt")), b"x");
		}
		let Reply::Ok(page) = request(
			&mut files,
			serde_json::json!({"op": "list", "root": "private", "path": "logs", "offset": 1, "limit": 2}),
		) else {
			panic!("list")
		};
		let page: serde_json::Value = serde_json::from_slice(&page).unwrap();
		let names: Vec<_> = page["entries"]
			.as_array()
			.unwrap()
			.iter()
			.map(|e| e["name"].as_str().unwrap().to_owned())
			.collect();
		assert_eq!(
			(names, page["more"].as_bool()),
			(vec!["a.txt".to_owned(), "b.txt".to_owned()], Some(true))
		);
		assert!(
			matches!(
				request(
					&mut files,
					serde_json::json!({"op": "rename", "root": "private", "path": "logs/a.txt", "to": "logs/b.txt"})
				),
				Reply::Failed(_)
			),
			"never over an existing entry"
		);
		let _ = fs::remove_dir_all(data);
	}

	/// A symbolic link under a root is never followed, so a link planted to the outside
	/// cannot be read through. Creating links needs privileges on Windows; the test then
	/// checks nothing.
	#[test]
	fn symbolic_links_under_a_root_are_not_followed() {
		let data = temp("links");
		let outside = temp("links-outside");
		fs::write(outside.join("secret.txt"), b"keep out").unwrap();
		let private = data.join(PRIVATE_FOLDER);
		fs::create_dir_all(&private).unwrap();
		#[cfg(unix)]
		let linked = std::os::unix::fs::symlink(&outside, private.join("out")).is_ok();
		#[cfg(windows)]
		let linked = std::os::windows::fs::symlink_dir(&outside, private.join("out")).is_ok();
		if linked {
			let mut files = PluginFiles::new(&data, 1024, Vec::new());
			assert!(matches!(
				request(
					&mut files,
					serde_json::json!({"op": "read", "root": "private", "path": "out/secret.txt", "length": 100})
				),
				Reply::Failed(_)
			));
			assert!(matches!(
				write(
					&mut files,
					serde_json::json!({"root": "private", "path": "out/new.txt"}),
					b"x"
				),
				Reply::Failed(_)
			));
			assert!(!outside.join("new.txt").exists());
		}
		let _ = fs::remove_dir_all(data);
		let _ = fs::remove_dir_all(outside);
	}
}
