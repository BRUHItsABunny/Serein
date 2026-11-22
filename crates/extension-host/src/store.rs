//! Bounded extension work. No package IO or Wasm runs on the UI thread.
use std::{
	collections::VecDeque,
	fs::{self, File, OpenOptions},
	io::{Cursor, Read, Write},
	net::{IpAddr, Ipv4Addr},
	path::{Path, PathBuf},
	sync::{
		Arc,
		atomic::{AtomicU64, Ordering},
		mpsc::{self, Receiver},
	},
	time::Duration,
};

use extensions::{
	Capability, Catalog, CatalogEntry, ExtensionKind, Invocation, Manifest, Output, Package,
	Preview, Surface, Theme,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::broker::{Broker, Subscription, TopicSession};
use crate::database::{self, PluginDatabase};
use crate::hooks::ChannelHooks;
use crate::journal::{self, JournalSession};
use crate::network::PluginNetwork;
use crate::secrets;

/// Generated from `extensions/` and pinned to a merged commit by the catalog workflow.
const CATALOG_URL: &str =
	"https://raw.githubusercontent.com/ViceVerse-cz/Serein/main/extensions/catalog.json";
const MAX_PACKAGE: usize = 16 * 1024 * 1024;
const MAX_RECORD: usize = MAX_PACKAGE + 64 * 1024;
const MAX_CATALOG: usize = 1024 * 1024;
const MAX_STORAGE: usize = 1024 * 1024;
const MAX_PER_SCOPE: usize = 8;
const MAX_ACCOUNTS: usize = 8;
const MAX_QUEUE: usize = 4;

pub fn trim_extended_input(invocation: &mut Invocation, limit: usize) -> bool {
	for field in 0..=3 {
		if serde_json::to_vec(invocation).is_ok_and(|wire| wire.len() <= limit) {
			return true;
		}
		match field {
			0 => invocation.messaging_settings = None,
			1 => invocation.guild_folders = None,
			2 => invocation.queries = None,
			_ => return false,
		}
	}
	false
}

#[derive(Clone)]
pub enum InstallSource {
	Bundled {
		bytes: &'static [u8],
		sha256: String,
		manifest: Manifest,
	},
	Catalog(CatalogEntry),
	Local {
		path: PathBuf,
		sha256: String,
		manifest: Manifest,
	},
}

// One job crosses to the worker at a time; boxing the invocation would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum Job {
	EditTheme {
		id: String,
		preview: bool,
	},
	SaveTheme {
		package: Box<Package>,
	},
	ExportTheme {
		package: Box<Package>,
		path: PathBuf,
	},
	SelectTheme {
		id: Option<String>,
	},
	Load {
		account: Option<String>,
	},
	RefreshCatalog,
	Preview {
		id: String,
		preview: Preview,
	},
	InspectImport {
		path: PathBuf,
	},
	Enable {
		source: Box<InstallSource>,
		grants: Vec<Capability>,
		account: Option<String>,
	},
	Disable {
		id: String,
		kind: ExtensionKind,
		account: Option<String>,
		/// Also removes the plugin's data; otherwise it is kept for a later reinstall.
		delete_data: bool,
	},
	/// Reports the size and database quota of each plugin's data in the account's scopes.
	DataUsage {
		account: Option<String>,
	},
	/// Removes a plugin's kept data. `connection` selects the device-wide connection scope.
	DeleteData {
		id: String,
		account: Option<String>,
		connection: bool,
	},
	/// Approves or revokes one subscription between two of the account's plugins.
	SetSubscription {
		account: String,
		subscription: Subscription,
		approved: bool,
	},
	/// Replaces the message hooks switched on in one channel of the account.
	SetChannelHooks {
		account: String,
		hooks: ChannelHooks,
	},
	/// Saves or, with `None`, removes a secret the plugin's manifest declares.
	SetSecret {
		id: String,
		account: Option<String>,
		connection: bool,
		secret: String,
		value: Option<zeroize::Zeroizing<String>>,
	},
	/// Adds or edits a catalog source; an empty `id` adds one. `token`: `Some(Some)` saves a
	/// token, `Some(None)` forgets it, `None` keeps it unless the address or token origins
	/// changed, which forgets it too.
	SetCatalogSource {
		source: crate::sources::CatalogSource,
		token: Option<Option<zeroize::Zeroizing<String>>>,
	},
	RemoveCatalogSource {
		id: String,
	},
	/// Refreshes one source now, such as one that does not refresh with the built-in catalog.
	RefreshCatalogSource {
		id: String,
	},
	/// Gives a plugin with `files` the folder or file the user picked at `path` for `pick`.
	AddFileHandle {
		id: String,
		account: Option<String>,
		connection: bool,
		pick: extensions::FilePick,
		path: PathBuf,
	},
	/// Takes a picked folder or file away from a plugin.
	RemoveFileHandle {
		id: String,
		account: Option<String>,
		connection: bool,
		root: String,
	},
	/// Sets a plugin's database quota to one of [`database::QUOTAS`].
	SetDatabaseQuota {
		id: String,
		account: Option<String>,
		connection: bool,
		bytes: u64,
	},
	Invoke {
		id: String,
		account: String,
		invocation: Invocation,
	},
	Logout {
		account: String,
	},
}

#[derive(Clone)]
pub struct Starter {
	pub source: InstallSource,
	pub theme: Option<Theme>,
	pub description: &'static str,
	pub download_bytes: u64,
}

pub fn starters() -> Result<Vec<Starter>, String> {
	let packages: &[(&'static [u8], &'static str)] = &[
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!(
				"../../../extensions/plugins/packages/message-delete-protector.serein-extension"
			),
			"Keep messages already seen in this session visible in red after deletion. Cleared when disabled or signed out.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/ocean.serein-extension"),
			"Deep blue surfaces with a bright ocean accent.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/midnight.serein-extension"),
			"Inky midnight surfaces with a vivid violet accent.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/rose.serein-extension"),
			"Soft rose surfaces with a warm pink accent.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/forest.serein-extension"),
			"Calm forest greens and fresh leafy accents.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/latte.serein-extension"),
			"Warm coffee tones and a creamy caramel accent.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/golden.serein-extension"),
			"Warm charcoal and gold, with rounded, roomy controls.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/katana.serein-extension"),
			"Katana's dark charcoal surfaces and sharp red accents. Light mode uses built-in colors.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/obsidian.serein-extension"),
			"Obsidian violet surfaces and lavender accents in light and dark.",
		),
		#[cfg(any(test, feature = "demo"))]
		(
			include_bytes!("../../../extensions/themes/teal.serein-extension"),
			"Cool blue-green surfaces with fresh teal accents.",
		),
	];
	packages
		.iter()
		.copied()
		.map(|(bytes, description)| {
			let package = extensions::parse_package(bytes).map_err(|error| error.to_string())?;
			Ok(Starter {
				source: InstallSource::Bundled {
					bytes,
					sha256: digest(bytes),
					manifest: package.manifest,
				},
				theme: package.theme,
				description,
				download_bytes: bytes.len() as u64,
			})
		})
		.collect()
}

/// Offline debug smoke check of the exact starter packages embedded in the app.
#[cfg(feature = "demo")]
pub fn demo_check_examples() -> Result<bool, String> {
	let starters = starters()?;
	if starters.len() != 10
		|| starters
			.iter()
			.filter(|entry| entry.theme.is_some())
			.count() != 9
	{
		return Err("Expected one starter plugin and nine themes".into());
	}
	let gate = Gate::detached();
	let mut activated = false;
	for starter in starters {
		let InstallSource::Bundled {
			bytes,
			sha256,
			manifest,
		} = starter.source
		else {
			return Err("Starter source must be bundled".into());
		};
		if digest(bytes) != sha256 || bytes.len() as u64 != starter.download_bytes {
			return Err("Starter checksum or byte length changed".into());
		}
		let package = extensions::parse_package(bytes).map_err(|error| error.to_string())?;
		if package.manifest != manifest {
			return Err("Starter manifest changed".into());
		}
		let mut stored = Stored {
			local_theme: false,
			grants: manifest.capabilities.clone(),
			package,
			reviewed: true,
			sha256,
			download_bytes: starter.download_bytes,
		};
		let summary = stored.summary(&gate, None, None, true);
		if let Some(error) = summary.error {
			return Err(error);
		}
		if manifest.kind == ExtensionKind::Plugin {
			let ok = match manifest.id.as_str() {
				"message-delete-protector" => summary.preserve_deleted_messages,
				_ => false,
			};
			if !ok {
				return Err("Bundled plugin did not activate".into());
			}
			activated = true;
			stored.grants.clear();
			let denied = stored.summary(&gate, None, None, true);
			if denied.preserve_deleted_messages || denied.image_sharing || denied.error.is_none() {
				return Err("Plugin activation must require explicit permission".into());
			}
		} else if summary.preserve_deleted_messages
			|| summary.image_sharing
			|| summary.theme.is_none()
		{
			return Err("Theme starter must only supply a valid palette".into());
		}
	}
	let root = std::env::temp_dir().join(format!("serein-theme-check-{}", std::process::id()));
	fs::create_dir(&root).map_err(|_| "Cannot create isolated theme check directory")?;
	let result = (|| {
		for starter in self::starters()?
			.into_iter()
			.filter(|entry| entry.theme.is_some())
			.take(2)
		{
			enable(&root, starter.source, Vec::new(), None, &gate)?;
		}
		let installed = load(&root, None, &gate)?;
		assert_eq!(installed.len(), 2, "installing retains previous presets");
		let id = installed
			.iter()
			.find(|entry| !entry.active_theme)
			.unwrap()
			.manifest
			.id
			.clone();
		run(
			&root,
			Job::SelectTheme {
				id: Some(id.clone()),
			},
			&gate,
		)?;
		let reloaded = load(&root, None, &gate)?;
		assert_eq!(
			reloaded
				.iter()
				.find(|entry| entry.active_theme)
				.unwrap()
				.manifest
				.id,
			id
		);
		run(&root, Job::SelectTheme { id: None }, &gate)?;
		let reloaded = load(&root, None, &gate)?;
		assert_eq!(reloaded.len(), 2, "built-in presets keep installed themes");
		assert!(reloaded.iter().all(|entry| !entry.active_theme));
		Ok(activated)
	})();
	let cleanup =
		fs::remove_dir_all(&root).map_err(|_| "Cannot remove isolated theme check directory");
	cleanup?;
	result
}

#[derive(Clone)]
pub struct InstalledExtension {
	pub background_image: Option<Arc<egui::ColorImage>>,
	pub cover_image: Option<Arc<egui::ColorImage>>,
	pub local_theme: bool,
	pub active_theme: bool,
	pub manifest: Manifest,
	pub theme: Option<Theme>,
	pub reviewed: bool,
	pub sha256: String,
	pub download_bytes: u64,
	pub error: Option<String>,
	pub preserve_deleted_messages: bool,
	pub image_sharing: bool,
	pub rich_presence: Option<Box<extensions::CustomRichPresence>>,
	pub api_proxy: Option<extensions::ApiProxyConfig>,
	/// Interface tweaks the granted activation action switched on.
	pub interface_tweaks: Vec<extensions::InterfaceTweak>,
	/// Interface tree the granted plugin last returned, replacing the main panes while set.
	pub interface_ui: Option<extensions::InterfaceUi>,
	/// Start screen the granted plugin last restated this session: `Some(None)` cleared it and
	/// `None` means it restated nothing, so a saved one stays.
	pub start_screen: Option<Option<extensions::StartScreen>>,
}

/// One plugin's data directory: its size and database quota.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginData {
	pub id: String,
	/// Data of a connection plugin, which is device-wide rather than per account.
	pub connection: bool,
	pub bytes: u64,
	pub database_quota: u64,
	/// Secret IDs the user has set; values stay in the secret store.
	pub secrets: Vec<String>,
	/// Folders and files the user picked for the plugin.
	pub handles: Vec<extensions::FileHandleInfo>,
}

// Events reach the front end one at a time; boxing the output would only add an allocation.
#[allow(clippy::large_enum_variant)]
pub enum Event {
	ThemeSelected {
		id: Option<String>,
		background_image: Option<Arc<egui::ColorImage>>,
	},
	EditTheme {
		package: Box<Package>,
		image: Option<Arc<egui::ColorImage>>,
		cover: Option<Arc<egui::ColorImage>>,
		local_theme: bool,
		preview: bool,
	},
	ThemeExported,
	Loaded {
		catalog: Option<Catalog>,
		installed: Vec<InstalledExtension>,
		starters: Vec<Starter>,
		data: Vec<PluginData>,
		subscriptions: Vec<Subscription>,
		hooks: Vec<ChannelHooks>,
		/// Added catalog sources, as last cached.
		sources: Vec<crate::sources::SourceStatus>,
	},
	/// The built-in catalog merged with the added sources' catalogs.
	Catalog {
		catalog: Option<Catalog>,
		sources: Vec<crate::sources::SourceStatus>,
	},
	/// The added catalog sources changed.
	CatalogSources(Vec<crate::sources::SourceStatus>),
	Preview {
		id: String,
		image: Option<egui::ColorImage>,
	},
	Imported {
		manifest: Manifest,
		source: InstallSource,
		download_bytes: u64,
	},
	Enabled(InstalledExtension),
	Disabled(String),
	Invoked {
		id: String,
		output: Output,
	},
	/// Plugin data in the account's scopes, installed or kept from a removed plugin.
	DataUsage(Vec<PluginData>),
	/// A `topic` action finished; `more` when messages remain for another delivery.
	TopicDelivered {
		id: String,
		more: bool,
		/// Caption lines the subscriber returned, which the host shows in the call.
		captions: Option<Vec<extensions::Caption>>,
	},
	/// The account's approved plugin subscriptions.
	Subscriptions(Vec<Subscription>),
	/// The message hooks switched on in the account's channels.
	ChannelHooks(Vec<ChannelHooks>),
	/// The user's pick was saved as `root`; the plugin's `action` hears about it.
	FileHandleAdded {
		id: String,
		root: String,
		action: String,
		usage: Vec<PluginData>,
	},
	/// A `journal` action finished; `more` when entries remain for another delivery.
	JournalDelivered {
		id: String,
		more: bool,
	},
	LoggedOut,
}

/// Host services behind the `serein` Wasm imports, backed by the operating system and, with
/// the `database` grant, the plugin's own SQLite file.
pub struct SystemImports {
	database: Option<SharedDatabase>,
	journal: Option<SharedJournal>,
	network: Option<PluginNetwork>,
	topics: Option<SharedTopics>,
	files: Option<crate::files::PluginFiles>,
	archive: Option<ArchiveAccess>,
}

/// A `media_archive` plugin's way into the host's archive queue.
struct ArchiveAccess {
	archive: Arc<crate::archive::Archive>,
	data: PathBuf,
	quota: u64,
}

type SharedTopics = Arc<std::sync::Mutex<TopicSession>>;

type SharedDatabase = Arc<std::sync::Mutex<PluginDatabase>>;
type SharedJournal = Arc<std::sync::Mutex<JournalSession>>;

/// Work an invocation's host services did that the host keeps only once it accepts the
/// output: the plugin's database first, then its journal position, so a crash in between
/// redelivers entries instead of losing them.
#[derive(Default)]
struct Pending {
	database: Option<SharedDatabase>,
	journal: Option<SharedJournal>,
	topics: Option<SharedTopics>,
}

impl Pending {
	fn commit(self) -> Result<(), String> {
		if let Some(database) = self.database {
			database
				.lock()
				.map_err(|_| "Plugin database is unavailable".to_string())?
				.commit()?;
		}
		if let Some(journal) = self.journal {
			journal
				.lock()
				.map_err(|_| "Message journal is unavailable".to_string())?
				.commit()?;
		}
		if let Some(topics) = self.topics
			&& let Ok(mut topics) = topics.lock()
		{
			topics.commit();
		}
		Ok(())
	}

	fn topics_more(&self) -> bool {
		self.topics
			.as_ref()
			.and_then(|topics| topics.lock().ok().map(|topics| topics.more()))
			.unwrap_or(false)
	}

	fn journal_more(&self) -> bool {
		self.journal
			.as_ref()
			.and_then(|journal| journal.lock().ok().map(|journal| journal.more()))
			.unwrap_or(false)
	}
}

impl SystemImports {
	/// Services for one invocation of `stored`, installed in `directory`, plus the database
	/// session the caller commits once the invocation's output has been accepted.
	fn for_plugin(stored: &Stored, directory: Option<&Path>, gate: &Gate) -> (Box<Self>, Pending) {
		let database = directory
			.filter(|_| stored.grants.contains(&Capability::Database))
			.map(|directory| {
				Arc::new(std::sync::Mutex::new(PluginDatabase::new(
					directory.join(database::FILE_NAME),
					database_quota(directory),
				)))
			});
		// Only account plugins have a journal: it lives beside their data in the account scope.
		let journal = directory
			.filter(|_| {
				stored.grants.contains(&Capability::MessageJournal)
					&& !is_proxy(&stored.package.manifest)
			})
			.and_then(Path::parent)
			.map(|scope| {
				Arc::new(std::sync::Mutex::new(JournalSession::new(
					scope.join(journal::FILE_NAME),
					stored.package.manifest.id.clone(),
				)))
			});
		let manifest = &stored.package.manifest;
		// Topics connect the account's own plugins; connection plugins have none.
		let topics = (!is_proxy(manifest)
			&& (!manifest.broadcasts.is_empty() || !manifest.subscribes.is_empty()))
		.then(|| {
			Arc::new(std::sync::Mutex::new(TopicSession::new(
				gate.broker.clone(),
				manifest.id.clone(),
				manifest.broadcasts.clone(),
				!manifest.subscribes.is_empty(),
			)))
		});
		(
			Box::new(Self {
				database: database.clone(),
				journal: journal.clone(),
				topics: topics.clone(),
				archive: directory
					.filter(|_| {
						stored.grants.contains(&Capability::MediaArchive)
							&& stored.grants.contains(&Capability::Files)
					})
					.map(|directory| ArchiveAccess {
						archive: Arc::clone(&gate.archive),
						data: directory.to_path_buf(),
						quota: database_quota(directory),
					}),
				files: directory
					.filter(|_| stored.grants.contains(&Capability::Files))
					.map(|directory| {
						crate::files::PluginFiles::new(
							directory,
							database_quota(directory),
							read_settings(directory).handles,
						)
					}),
				network: stored.grants.contains(&Capability::Network).then(|| {
					let mut network =
						PluginNetwork::new(&stored.package.manifest.network, gate.clone());
					if let Some(directory) = directory {
						let directory = directory.to_path_buf();
						network.secrets(
							stored.package.manifest.secrets.clone(),
							Box::new(move |secret| {
								secrets::store()?.load(&secret_key(&directory, secret)?)
							}),
						);
					}
					network
				}),
			}),
			Pending {
				database,
				journal,
				topics,
			},
		)
	}
}

impl extensions::HostImports for SystemImports {
	fn now_ms(&mut self) -> Option<i64> {
		let elapsed = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.ok()?;
		i64::try_from(elapsed.as_millis()).ok()
	}
	fn fill_random(&mut self, out: &mut [u8]) -> bool {
		getrandom::fill(out).is_ok()
	}
	fn db_execute(&mut self, request: &[u8]) -> extensions::Reply {
		match &self.database {
			Some(database) => match database.lock() {
				Ok(mut database) => database.execute(request),
				Err(_) => extensions::Reply::Unavailable,
			},
			None => extensions::Reply::Unavailable,
		}
	}
	fn https_request(&mut self, request: &[u8]) -> extensions::Reply {
		match &mut self.network {
			Some(network) => network.request(request),
			None => extensions::Reply::Unavailable,
		}
	}
	fn broadcast(&mut self, request: &[u8]) -> i32 {
		match self.topics.as_ref().and_then(|topics| topics.lock().ok()) {
			Some(mut topics) => topics.broadcast(request),
			None => extensions::imports::status::UNAVAILABLE,
		}
	}
	fn has_subscribers(&mut self, topic: &str) -> i32 {
		match self.topics.as_ref().and_then(|topics| topics.lock().ok()) {
			Some(topics) => topics.has_subscribers(topic),
			None => extensions::imports::status::UNAVAILABLE,
		}
	}
	fn file_request(&mut self, request: &[u8]) -> extensions::Reply {
		if matches!(
			serde_json::from_slice::<extensions::FileRequest>(request),
			Ok(extensions::FileRequest::Fetches)
		) {
			let Some(access) = &self.archive else {
				return extensions::Reply::Unavailable;
			};
			let fetches: Vec<_> = access
				.archive
				.results(&access.data)
				.into_iter()
				.map(|(path, state)| match state {
					crate::archive::FetchState::Queued => {
						serde_json::json!({"path": path, "state": "queued"})
					}
					crate::archive::FetchState::Done => {
						serde_json::json!({"path": path, "state": "done"})
					}
					crate::archive::FetchState::Failed(error) => {
						serde_json::json!({"path": path, "state": "failed", "error": error})
					}
				})
				.collect();
			return serde_json::to_vec(&serde_json::json!({ "fetches": fetches })).map_or(
				extensions::Reply::Failed("Cannot encode the reply".into()),
				extensions::Reply::Ok,
			);
		}
		match &mut self.files {
			Some(files) => files.request(request),
			None => extensions::Reply::Unavailable,
		}
	}
	fn attachment_fetch(&mut self, request: &[u8]) -> extensions::Reply {
		let (Some(access), Some(files)) = (&self.archive, &self.files) else {
			return extensions::Reply::Unavailable;
		};
		let Ok(request) = serde_json::from_slice::<extensions::AttachmentFetch>(request) else {
			return extensions::Reply::Failed("Invalid attachment fetch".into());
		};
		let Some(url) = crate::archive::cdn_url(&request.url) else {
			return extensions::Reply::Failed(
				"Only Discord attachment links can be fetched".into(),
			);
		};
		if request.size == 0 || request.size > extensions::files::MAX_ARCHIVE_FILE_BYTES {
			return extensions::Reply::Failed(
				"Attachments must be nonempty and at most 100 MiB".into(),
			);
		}
		let target = match files.private_target(&request.path) {
			Ok(target) => target,
			Err(error) => return extensions::Reply::Failed(error.into()),
		};
		let fetch = crate::archive::Fetch {
			url,
			size: request.size,
			target,
			path: request.path,
			private: access.data.join(crate::files::PRIVATE_FOLDER),
			quota: access.quota,
		};
		match access.archive.queue(&access.data, fetch) {
			Ok(()) => extensions::Reply::Ok(Vec::new()),
			Err(error) => extensions::Reply::Failed(error),
		}
	}
	fn file_write(&mut self, header: &[u8], data: &[u8]) -> extensions::Reply {
		match &mut self.files {
			Some(files) => files.write(header, data),
			None => extensions::Reply::Unavailable,
		}
	}
	fn topic_read(&mut self, max: u32) -> extensions::Reply {
		match self.topics.as_ref().and_then(|topics| topics.lock().ok()) {
			Some(mut topics) => topics.read(max),
			None => extensions::Reply::Unavailable,
		}
	}
	fn journal_read(&mut self, max_entries: u32) -> extensions::Reply {
		match &self.journal {
			Some(journal) => match journal.lock() {
				Ok(mut journal) => journal.read(max_entries),
				Err(_) => extensions::Reply::Unavailable,
			},
			None => extensions::Reply::Unavailable,
		}
	}
}

/// Called from the worker thread after a job finishes so the front end polls its result,
/// for example by requesting a repaint.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// A single worker exists only while a job runs. Queued jobs and results have fixed limits.
pub struct ExtensionHost {
	root: PathBuf,
	broker: Arc<Broker>,
	archive: Arc<crate::archive::Archive>,
	generation: Arc<AtomicU64>,
	wake_cancel: Arc<tokio::sync::Notify>,
	queue: VecDeque<(u64, Job)>,
	next_token: u64,
	active: Option<(u64, Receiver<Result<Event, String>>)>,
	wake: Option<Wake>,
}

impl ExtensionHost {
	pub fn new(root: PathBuf) -> Self {
		Self {
			root,
			broker: Arc::new(Broker::default()),
			archive: Arc::default(),
			generation: Arc::new(AtomicU64::new(0)),
			wake_cancel: Arc::new(tokio::sync::Notify::new()),
			queue: VecDeque::new(),
			next_token: 0,
			active: None,
			wake: None,
		}
	}

	pub fn cancel(&mut self) {
		self.generation.fetch_add(1, Ordering::AcqRel);
		self.wake_cancel.notify_waiters();
		self.queue
			.retain(|(_, job)| matches!(job, Job::Disable { .. } | Job::Logout { .. }));
	}

	pub fn submit(&mut self, job: Job, wake: &Wake) -> Result<u64, String> {
		if matches!(job, Job::Disable { .. } | Job::Logout { .. }) {
			self.cancel();
		}
		if self.queue.len() >= MAX_QUEUE {
			return Err("Extensions are busy; try again after the current operation".into());
		}
		validate_job(&job)?;
		self.wake = Some(wake.clone());
		let token = self.next_token;
		self.next_token = self.next_token.wrapping_add(1);
		self.queue.push_back((token, job));
		self.start_next();
		Ok(token)
	}

	/// The broker plugin broadcasts go through; the front end sets its subscriptions.
	/// Starts `id`'s media worker for `account`. The worker thread loads the plugin, which
	/// must hold the `media_worker` grant and declare a `media` action. It gets the clock,
	/// randomness, its files, its network origins and broadcasts; no database, journal or
	/// topic deliveries.
	pub fn start_media(
		&self,
		id: &str,
		account: &str,
		wake: Arc<dyn Fn() + Send + Sync>,
	) -> crate::media::MediaWorker {
		let root = self.root.clone();
		let gate = Gate {
			epoch: 0,
			generation: Arc::new(AtomicU64::new(0)),
			wake_cancel: Arc::clone(&self.wake_cancel),
			broker: Arc::clone(&self.broker),
			archive: Arc::clone(&self.archive),
		};
		let (id, account) = (id.to_owned(), account.to_owned());
		crate::media::MediaWorker::spawn(
			Box::new(move || {
				let parent = scope(&root, ExtensionKind::Plugin, Some(&account))?;
				let stored = read_stored(&parent.join(&id).join("package.json"))?;
				if !stored.grants.contains(&Capability::MediaWorker) {
					return Err("The media worker was not granted".into());
				}
				let action = stored
					.package
					.manifest
					.actions
					.iter()
					.find(|action| action.surface == Surface::Media)
					.map(|action| action.id.clone())
					.ok_or("The plugin has no media action")?;
				let data = data_dir(&root, &id, Some(&account))?;
				let (mut imports, _) = SystemImports::for_plugin(&stored, Some(&data), &gate);
				// Batches have no invocation to commit with, so nothing waits for one.
				imports.database = None;
				imports.journal = None;
				if let Some(topics) = imports.topics.take() {
					drop(topics);
					let manifest = &stored.package.manifest;
					if !manifest.broadcasts.is_empty() {
						imports.topics = Some(Arc::new(std::sync::Mutex::new(TopicSession::new(
							Arc::clone(&gate.broker),
							manifest.id.clone(),
							manifest.broadcasts.clone(),
							false,
						))));
					}
				}
				let instance =
					extensions::MediaInstance::new(stored.package, &stored.grants, imports)
						.map_err(|error| error.to_string())?;
				Ok((instance, action))
			}),
			wake,
		)
	}
	pub fn broker(&self) -> Arc<Broker> {
		self.broker.clone()
	}

	/// The message journal shared by `account`'s plugins, kept with their data.
	pub fn journal_path(&self, account: &str) -> Result<PathBuf, String> {
		Ok(data_scope(&self.root, Some(account))?.join(journal::FILE_NAME))
	}

	pub fn busy(&self) -> bool {
		self.active.is_some() || !self.queue.is_empty()
	}

	pub fn poll(&mut self) -> Option<(u64, Result<Event, String>)> {
		let (token, receiver) = self.active.as_ref()?;
		let token = *token;
		let result = receiver.try_recv();
		match result {
			Ok(result) => {
				self.active = None;
				self.start_next();
				Some((token, result))
			}
			Err(mpsc::TryRecvError::Empty) => None,
			Err(mpsc::TryRecvError::Disconnected) => {
				self.active = None;
				self.start_next();
				Some((token, Err("Extension worker stopped unexpectedly".into())))
			}
		}
	}

	fn start_next(&mut self) {
		if self.active.is_some() {
			return;
		}
		let Some((token, job)) = self.queue.pop_front() else {
			return;
		};
		let (sender, receiver) = mpsc::sync_channel(1);
		let root = self.root.clone();
		let generation = if matches!(job, Job::Disable { .. } | Job::Logout { .. }) {
			Arc::new(AtomicU64::new(0))
		} else {
			self.generation.clone()
		};
		let gate = Gate {
			epoch: generation.load(Ordering::Acquire),
			generation,
			wake_cancel: self.wake_cancel.clone(),
			broker: self.broker.clone(),
			archive: self.archive.clone(),
		};
		let wake = self.wake.clone();
		self.active = Some((token, receiver));
		if std::thread::Builder::new()
			.name("serein-extension".into())
			.spawn(move || {
				let result = run(&root, job, &gate);
				let _ = sender.send(result);
				if let Some(wake) = wake {
					wake();
				}
			})
			.is_err()
		{
			// The disconnected receiver reports the failure through the ordinary error UI.
			if let Some(wake) = &self.wake {
				wake();
			}
		}
	}
}

impl Drop for ExtensionHost {
	fn drop(&mut self) {
		self.cancel();
	}
}

/// The job's cancellation state: it fails once the host's generation moves on.
#[derive(Clone)]
pub(crate) struct Gate {
	epoch: u64,
	generation: Arc<AtomicU64>,
	pub(crate) wake_cancel: Arc<tokio::sync::Notify>,
	/// The host's broker for plugin broadcasts.
	pub(crate) broker: Arc<Broker>,
	/// The host's attachment archive queue.
	pub(crate) archive: Arc<crate::archive::Archive>,
}
impl Gate {
	/// A gate no job cancellation reaches, for device files written outside the job queue.
	pub(crate) fn detached() -> Self {
		Self {
			epoch: 0,
			generation: Arc::new(AtomicU64::new(0)),
			wake_cancel: Arc::new(tokio::sync::Notify::new()),
			broker: Arc::new(Broker::default()),
			archive: Arc::default(),
		}
	}
	pub(crate) fn check(&self) -> Result<(), String> {
		if self.epoch == self.generation.load(Ordering::Acquire) {
			Ok(())
		} else {
			Err("Extension operation cancelled".into())
		}
	}
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
	#[serde(default)]
	local_theme: bool,
	package: Package,
	grants: Vec<Capability>,
	reviewed: bool,
	sha256: String,
	download_bytes: u64,
}

impl Stored {
	/// Builds the installed entry by running the activation action, if any, with saved storage
	/// and the plugin's database in `directory` (none for bundled checks), and records what its
	/// granted output switched on.
	fn summary(
		&self,
		gate: &Gate,
		storage: Option<String>,
		directory: Option<&Path>,
		active: bool,
	) -> InstalledExtension {
		let activation = self
			.package
			.manifest
			.actions
			.iter()
			.find(|action| action.surface == Surface::Activation);
		let mut result = activation.map_or(Ok(extensions::Output::default()), |action| {
			validate_grants(&self.package.manifest, &self.grants)?;
			gate.check()?;
			let (services, pending) = SystemImports::for_plugin(self, directory, gate);
			let output = extensions::invoke_with(
				&self.package,
				&Invocation {
					action: action.id.clone(),
					storage,
					..Default::default()
				},
				&self.grants,
				services,
			)
			.map_err(|error| error.to_string())?;
			gate.check()?;
			if output.preserve_deleted_messages
				&& !self.grants.contains(&Capability::DeletedMessages)
			{
				return Err("Deleted message access was not granted".into());
			}
			if output.image_sharing && !self.grants.contains(&Capability::ImageSharing) {
				return Err("Image sharing access was not granted".into());
			}
			if output.interface_tweaks.is_some()
				&& !self.grants.contains(&Capability::InterfaceTweaks)
			{
				return Err("Interface tweaks were not granted".into());
			}
			if output.interface_ui.is_some() && !self.grants.contains(&Capability::InterfaceUi) {
				return Err("Interface replacement was not granted".into());
			}
			if output.start_screen.is_some() && !self.grants.contains(&Capability::StartScreen) {
				return Err("Start screen customization was not granted".into());
			}
			gate.check()?;
			pending.commit()?;
			Ok(output)
		});
		let background_image = if active {
			match package_background(&self.package) {
				Ok(image) => image,
				Err(error) => {
					result = Err(error);
					None
				}
			}
		} else {
			None
		};
		let cover_image = match package_cover(&self.package) {
			Ok(image) => image,
			Err(error) => {
				result = Err(error);
				None
			}
		};
		InstalledExtension {
			background_image,
			cover_image,
			local_theme: self.local_theme,
			active_theme: self.package.manifest.kind == ExtensionKind::Theme,
			manifest: self.package.manifest.clone(),
			theme: self.package.theme.clone().or_else(|| {
				result
					.as_ref()
					.ok()
					.and_then(|output| output.appearance.clone())
			}),
			reviewed: self.reviewed,
			sha256: self.sha256.clone(),
			download_bytes: self.download_bytes,
			image_sharing: result.as_ref().is_ok_and(|output| output.image_sharing),
			api_proxy: result
				.as_ref()
				.ok()
				.and_then(|output| output.api_proxy.clone()),
			rich_presence: result
				.as_ref()
				.ok()
				.and_then(|output| match &output.rich_presence {
					Some(extensions::RichPresenceUpdate::Set { presence }) => {
						Some(presence.clone())
					}
					_ => None,
				}),
			interface_tweaks: result
				.as_ref()
				.ok()
				.and_then(|output| output.interface_tweaks.clone())
				.unwrap_or_default(),
			interface_ui: result
				.as_ref()
				.ok()
				.and_then(|output| output.interface_ui.clone()),
			start_screen: result
				.as_ref()
				.ok()
				.and_then(|output| output.start_screen.clone()),
			// Current protector packages have a no-op activation; consent still opts in.
			preserve_deleted_messages: activation.is_some()
				&& result.is_ok()
				&& self.grants.contains(&Capability::DeletedMessages),
			error: result.err(),
		}
	}
}

/// Rejects a job whose IDs, account or invocation fail the host's limits before it reaches
/// the worker, so a malformed request never runs plugin code.
fn validate_job(job: &Job) -> Result<(), String> {
	match job {
		Job::Invoke {
			id,
			account,
			invocation,
		} => {
			valid_id(id)?;
			if !account.is_empty() {
				account_key(account)?;
			}
			let event_bytes = if let Some(event) = &invocation.message_event {
				event.validate().map_err(|error| error.to_string())?;
				event
					.channel_id
					.len()
					.saturating_add(event.message_id.len())
					.saturating_add(event.author_id.as_ref().map_or(0, String::len))
					.saturating_add(event.content.as_ref().map_or(0, String::len))
			} else {
				0
			};
			if invocation.values.len() > 64
				|| invocation
					.action
					.len()
					.saturating_add(invocation.selected_message.as_ref().map_or(0, String::len))
					.saturating_add(
						invocation
							.selected_message_ref
							.as_ref()
							.map_or(0, |reference| reference.bytes()),
					)
					.saturating_add(
						invocation
							.menu_target
							.as_ref()
							.map_or(0, |target| target.bytes()),
					)
					.saturating_add(invocation.composer.as_ref().map_or(0, String::len))
					.saturating_add(invocation.storage.as_ref().map_or(0, String::len))
					.saturating_add(event_bytes)
					.saturating_add(
						invocation
							.app
							.as_ref()
							.map_or(0, |app| app.bytes().unwrap_or(usize::MAX)),
					)
					.saturating_add(
						invocation
							.values
							.iter()
							.map(|(key, value)| key.len().saturating_add(value.len()))
							.sum::<usize>(),
					) > 256 * 1024
			{
				return Err("Plugin input exceeds 256 KiB".into());
			}
		}
		Job::Preview { id, preview, .. } => {
			valid_id(id)?;
			preview.validate().map_err(|e| e.to_string())?;
		}
		Job::SelectTheme { id: Some(id) }
		| Job::Disable { id, .. }
		| Job::EditTheme { id, .. }
		| Job::DeleteData { id, .. } => valid_id(id)?,
		Job::SetSecret {
			id, secret, value, ..
		} => {
			valid_id(id)?;
			if extensions::network::placeholders(&format!("{{{{secret.{secret}}}}}"))
				.is_none_or(|ids| ids.len() != 1)
				|| value
					.as_ref()
					.is_some_and(|value| !secrets::valid_value(value))
			{
				return Err("Secrets are 1 to 4,096 characters without line breaks".into());
			}
		}
		Job::SetChannelHooks { hooks, .. } => {
			hooks.validate()?;
			for plugin in hooks.plugins() {
				valid_id(plugin)?;
			}
		}
		Job::SetSubscription { subscription, .. } => {
			valid_id(&subscription.subscriber)?;
			valid_id(&subscription.provider)?;
			if subscription.subscriber == subscription.provider
				|| !extensions::topics::valid_name(&subscription.topic)
			{
				return Err("Invalid subscription".into());
			}
		}
		Job::AddFileHandle { id, pick, .. } => {
			valid_id(id)?;
			pick.validate(|_| true).map_err(|error| error.to_string())?;
		}
		Job::RemoveFileHandle { id, .. } => valid_id(id)?,
		Job::SetDatabaseQuota { id, bytes, .. } => {
			valid_id(id)?;
			if !database::QUOTAS.contains(bytes) {
				return Err("Choose one of the offered database sizes".into());
			}
		}
		Job::SaveTheme { package } | Job::ExportTheme { package, .. } => {
			if package.manifest.kind != ExtensionKind::Theme {
				return Err("Theme editor only accepts themes".into());
			}
			package.validate().map_err(|error| error.to_string())?;
			if let Job::ExportTheme { path, .. } = job
				&& path.as_os_str().len() > 4096
			{
				return Err("Export path is too long".into());
			}
		}
		Job::Enable { grants, .. } if grants.len() > extensions::MAX_CAPABILITIES => {
			return Err("Invalid plugin grants".into());
		}
		Job::InspectImport { path } if path.as_os_str().len() > 4096 => {
			return Err("Import path is too long".into());
		}
		_ => {}
	}
	Ok(())
}

/// Runs one validated job on the worker thread and returns its event; the gate aborts work
/// that a newer job or logout made stale.
fn run(root: &Path, job: Job, gate: &Gate) -> Result<Event, String> {
	gate.check()?;
	match job {
		Job::EditTheme { id, preview } => {
			valid_id(&id)?;
			let directory = root.join("themes").join(&id);
			if directory.with_extension("disabled").exists() {
				return Err("Theme cleanup is pending".into());
			}
			let (package, local_theme) = if directory.exists() {
				let stored = read_stored(&directory.join("package.json"))?;
				(stored.package, stored.local_theme)
			} else {
				let starter = starters()?
					.into_iter()
					.find(|starter| {
						matches!(&starter.source, InstallSource::Bundled { manifest, .. }
						if manifest.id == id && manifest.kind == ExtensionKind::Theme)
					})
					.ok_or("Theme is not available locally")?;
				let InstallSource::Bundled { bytes, .. } = starter.source else {
					return Err("Theme is not bundled".into());
				};
				(
					extensions::parse_package(bytes).map_err(|error| error.to_string())?,
					false,
				)
			};
			if package.manifest.id != id || package.manifest.kind != ExtensionKind::Theme {
				return Err("Theme does not match the requested package".into());
			}
			let image = package_background(&package)?;
			let cover = package_cover(&package)?;
			gate.check()?;
			Ok(Event::EditTheme {
				local_theme,
				preview,
				package: Box::new(package),
				image,
				cover,
			})
		}
		Job::SaveTheme { package } => {
			let bytes = encode_theme(&package)?;
			let _ = package_cover(&package)?;
			let directory = root.join("themes").join(&package.manifest.id);
			if directory.exists() && !read_stored(&directory.join("package.json"))?.local_theme {
				return Err(
					"This theme belongs to another package; duplicate it before saving".into(),
				);
			}
			let stored = Stored {
				package: *package,
				grants: Vec::new(),
				reviewed: false,
				local_theme: true,
				sha256: digest(&bytes),
				download_bytes: bytes.len() as u64,
			};
			install_stored(root, stored, None, gate).map(Event::Enabled)
		}
		Job::ExportTheme { package, path } => {
			let bytes = encode_theme(&package)?;
			// Reject invalid images before creating or replacing the user's export.
			let _ = package_background(&package)?;
			let _ = package_cover(&package)?;
			export_theme(&path, &bytes, gate)?;
			Ok(Event::ThemeExported)
		}
		Job::SelectTheme { id } => {
			let parent = root.join("themes");
			let mut background_image = None;
			if let Some(id) = &id {
				valid_id(id)?;
				let stored = read_stored(&parent.join(id).join("package.json"))?;
				if stored.package.manifest.id != *id
					|| stored.package.manifest.kind != ExtensionKind::Theme
					|| parent.join(id).with_extension("disabled").exists()
				{
					return Err("Theme is not installed".into());
				}
				background_image = package_background(&stored.package)?;
				atomic_write(&parent.join("active.json"), id.as_bytes(), gate)?;
			} else {
				remove_file(&parent.join("active.json"))?;
			}
			Ok(Event::ThemeSelected {
				id,
				background_image,
			})
		}
		Job::Load { account } => Ok(Event::Loaded {
			catalog: merged_catalog(root, cached_official(root)),
			sources: source_statuses(root, Vec::new()),
			starters: starters()?,
			installed: load(root, account.as_deref(), gate)?,
			data: data_usage(root, account.as_deref())?,
			subscriptions: match account.as_deref() {
				Some(account) => read_subscriptions(root, account),
				None => Vec::new(),
			},
			hooks: match account.as_deref() {
				Some(account) => read_hooks(root, account),
				None => Vec::new(),
			},
		}),
		Job::RefreshCatalog => {
			let bytes = download(
				CATALOG_URL,
				MAX_CATALOG,
				gate,
				Duration::from_secs(15),
				None,
			)?;
			let official = cache_catalog(root, &bytes, gate)?;
			let errors = refresh_sources(root, gate, |source| source.auto_refresh);
			Ok(Event::Catalog {
				catalog: merged_catalog(root, Some(official)),
				sources: source_statuses(root, errors),
			})
		}
		Job::RefreshCatalogSource { id } => {
			let errors = refresh_sources(root, gate, |source| source.id == id);
			Ok(Event::Catalog {
				catalog: merged_catalog(root, cached_official(root)),
				sources: source_statuses(root, errors),
			})
		}
		Job::SetCatalogSource { mut source, token } => {
			let mut sources = crate::sources::read(root);
			if source.id.is_empty() {
				if sources.len() >= crate::sources::MAX_SOURCES {
					return Err("Remove a catalog source before adding another".into());
				}
				source.id = crate::sources::new_id(&source.name, &sources);
			}
			source.validate()?;
			let key = crate::sources::token_key(&source.id);
			let existing = sources.iter().position(|known| known.id == source.id);
			let moved = existing.is_some_and(|index| {
				sources[index].url != source.url
					|| sources[index].token_origins != source.token_origins
			});
			source.has_token = match token {
				Some(Some(token)) => {
					if !secrets::valid_value(&token) {
						return Err("Paste a token of at most 4 KiB without line breaks".into());
					}
					secrets::store()?.save(&key, &token)?;
					true
				}
				None if !moved => existing.is_some_and(|index| sources[index].has_token),
				_ => {
					if existing.is_some_and(|index| sources[index].has_token) {
						secrets::store()?.forget(&key)?;
					}
					false
				}
			};
			if moved {
				let _ = fs::remove_file(crate::sources::cache_path(root, &source.id));
			}
			match existing {
				Some(index) => sources[index] = source,
				None => sources.push(source),
			}
			write_sources(root, &sources, gate)?;
			Ok(Event::CatalogSources(source_statuses(root, Vec::new())))
		}
		Job::RemoveCatalogSource { id } => {
			let mut sources = crate::sources::read(root);
			if let Some(index) = sources.iter().position(|source| source.id == id) {
				if sources[index].has_token {
					secrets::store()?.forget(&crate::sources::token_key(&id))?;
				}
				sources.remove(index);
				let _ = fs::remove_file(crate::sources::cache_path(root, &id));
				write_sources(root, &sources, gate)?;
			}
			Ok(Event::CatalogSources(source_statuses(root, Vec::new())))
		}
		Job::Preview { id, preview } => {
			let image = load_preview(&preview, gate);
			Ok(Event::Preview { id, image })
		}
		Job::InspectImport { path } => {
			let bytes = read_bounded(&path, MAX_PACKAGE)?;
			let package = extensions::parse_package(&bytes).map_err(|e| e.to_string())?;
			let manifest = package.manifest;
			Ok(Event::Imported {
				source: InstallSource::Local {
					path,
					sha256: digest(&bytes),
					manifest: manifest.clone(),
				},
				manifest,
				download_bytes: bytes.len() as u64,
			})
		}
		Job::Enable {
			source,
			grants,
			account,
		} => enable(root, *source, grants, account.as_deref(), gate).map(Event::Enabled),
		Job::Disable {
			id,
			kind,
			account,
			delete_data,
		} => {
			let parent = scope(root, kind, account.as_deref())?;
			disable(&parent, &id, gate)?;
			if kind == ExtensionKind::Plugin
				&& let Ok(directory) = data_dir(root, &id, account.as_deref())
			{
				gate.archive.forget(&directory);
			}
			// Disabling either side of a subscription ends it.
			if let Some(account) = account.as_deref().filter(|_| kind == ExtensionKind::Plugin) {
				let mut subscriptions = read_subscriptions(root, account);
				let before = subscriptions.len();
				subscriptions.retain(|subscription| {
					subscription.subscriber != id && subscription.provider != id
				});
				if subscriptions.len() != before {
					write_subscriptions(root, account, &subscriptions, gate)?;
				}
				// And switches its hooks off everywhere.
				let mut hooks = read_hooks(root, account);
				let changed = hooks
					.iter_mut()
					.fold(false, |changed, hooks| hooks.remove(&id) | changed);
				if changed {
					hooks.retain(|hooks| !hooks.is_empty());
					write_hooks(root, account, &hooks, gate)?;
				}
			}
			if delete_data && kind == ExtensionKind::Plugin {
				delete_data_dir(&data_dir(root, &id, account.as_deref())?)?;
			}
			Ok(Event::Disabled(id))
		}
		Job::DataUsage { account } => data_usage(root, account.as_deref()).map(Event::DataUsage),
		Job::SetSubscription {
			account,
			subscription,
			approved,
		} => {
			let mut subscriptions = read_subscriptions(root, &account);
			subscriptions.retain(|existing| *existing != subscription);
			if approved {
				let parent = scope(root, ExtensionKind::Plugin, Some(&account))?;
				let manifest = |id: &str| {
					read_stored(&parent.join(id).join("package.json"))
						.map(|stored| stored.package.manifest)
				};
				let topic = &subscription.topic;
				if !manifest(&subscription.subscriber)?
					.subscribes
					.contains(topic)
					|| !manifest(&subscription.provider)?.broadcasts.contains(topic)
				{
					return Err("Those plugins do not share that topic".into());
				}
				if subscriptions.len() >= MAX_SUBSCRIPTIONS {
					return Err("Too many plugin subscriptions; revoke one first".into());
				}
				subscriptions.push(subscription);
			}
			write_subscriptions(root, &account, &subscriptions, gate)?;
			Ok(Event::Subscriptions(subscriptions))
		}
		Job::SetChannelHooks { account, hooks } => {
			let parent = scope(root, ExtensionKind::Plugin, Some(&account))?;
			let has = |id: &str, surface: Surface, capability: Capability| {
				read_stored(&parent.join(id).join("package.json")).is_ok_and(|stored| {
					stored.grants.contains(&capability)
						&& stored
							.package
							.manifest
							.actions
							.iter()
							.any(|action| action.surface == surface)
				})
			};
			if hooks
				.send
				.as_ref()
				.is_some_and(|hook| !has(&hook.plugin, Surface::Send, Capability::SendHook))
				|| hooks
					.annotate
					.iter()
					.any(|plugin| !has(plugin, Surface::Annotate, Capability::MessageAnnotations))
			{
				return Err("That plugin has no hook for messages".into());
			}
			let mut saved = read_hooks(root, &account);
			saved.retain(|existing| existing.channel != hooks.channel);
			if !hooks.is_empty() {
				if saved.len() >= crate::hooks::MAX_CHANNELS {
					return Err("Too many channels with plugin hooks; switch some off first".into());
				}
				saved.push(hooks);
			}
			write_hooks(root, &account, &saved, gate)?;
			Ok(Event::ChannelHooks(saved))
		}
		Job::DeleteData {
			id,
			account,
			connection,
		} => {
			let account = if connection { None } else { account };
			if account.is_none() && !connection {
				return Err("Sign in to manage this account's plugin data".into());
			}
			delete_data_dir(&data_dir(root, &id, account.as_deref())?)?;
			data_usage(root, account.as_deref()).map(Event::DataUsage)
		}
		Job::AddFileHandle {
			id,
			account,
			connection,
			pick,
			path,
		} => {
			let account = if connection { None } else { account };
			let parent = scope(root, ExtensionKind::Plugin, account.as_deref())?;
			let stored = read_stored(&parent.join(&id).join("package.json"))?;
			if !stored.grants.contains(&Capability::Files) {
				return Err("File access was not granted".into());
			}
			let picked = match pick.kind {
				extensions::PickKind::Folder => path.is_dir(),
				extensions::PickKind::File => path.is_file(),
				extensions::PickKind::SaveFile => {
					path.parent().is_some_and(Path::is_dir) && !path.is_dir()
				}
			};
			if !path.is_absolute()
				|| !picked || fs::symlink_metadata(&path)
				.is_ok_and(|meta| meta.file_type().is_symlink())
			{
				return Err("Pick a folder or file on this device".into());
			}
			let directory = data_dir(root, &id, account.as_deref())?;
			let mut settings = read_settings(&directory);
			let handle = crate::files::FileHandle {
				id: crate::files::new_handle_id(&settings.handles)
					.ok_or("This plugin already has 16 folders and files; remove one first")?,
				kind: pick.kind,
				write: pick.write || pick.kind == extensions::PickKind::SaveFile,
				path,
				purpose: pick.purpose,
			};
			let handle_root = handle.id.clone();
			settings.handles.push(handle);
			write_settings(&directory, &settings, gate)?;
			Ok(Event::FileHandleAdded {
				id,
				root: handle_root,
				action: pick.action,
				usage: data_usage(root, account.as_deref())?,
			})
		}
		Job::RemoveFileHandle {
			id,
			account,
			connection,
			root: handle,
		} => {
			let account = if connection { None } else { account };
			let directory = data_dir(root, &id, account.as_deref())?;
			let mut settings = read_settings(&directory);
			settings.handles.retain(|saved| saved.id != handle);
			write_settings(&directory, &settings, gate)?;
			data_usage(root, account.as_deref()).map(Event::DataUsage)
		}
		Job::SetDatabaseQuota {
			id,
			account,
			connection,
			bytes,
		} => {
			let account = if connection { None } else { account };
			if account.is_none() && !connection {
				return Err("Sign in to manage this account's plugin data".into());
			}
			let directory = data_dir(root, &id, account.as_deref())?;
			let mut settings = read_settings(&directory);
			settings.database_bytes = Some(bytes);
			write_settings(&directory, &settings, gate)?;
			data_usage(root, account.as_deref()).map(Event::DataUsage)
		}
		Job::SetSecret {
			id,
			account,
			connection,
			secret,
			value,
		} => {
			let account = if connection { None } else { account };
			let parent = scope(root, ExtensionKind::Plugin, account.as_deref())?;
			let stored = read_stored(&parent.join(&id).join("package.json"))?;
			if !stored
				.package
				.manifest
				.secrets
				.iter()
				.any(|spec| spec.id == secret)
			{
				return Err("This plugin does not declare that secret".into());
			}
			let directory = data_dir(root, &id, account.as_deref())?;
			let key = secret_key(&directory, &secret)?;
			let store = secrets::store()?;
			let mut settings = read_settings(&directory);
			settings.secrets.retain(|set| *set != secret);
			match value {
				Some(value) => {
					store.save(&key, &value)?;
					settings.secrets.push(secret);
				}
				None => store.forget(&key)?,
			}
			write_settings(&directory, &settings, gate)?;
			data_usage(root, account.as_deref()).map(Event::DataUsage)
		}
		Job::Invoke {
			id,
			account,
			mut invocation,
		} => {
			let directory = scope(
				root,
				ExtensionKind::Plugin,
				(!account.is_empty()).then_some(account.as_str()),
			)?
			.join(&id);
			if directory.with_extension("disabled").exists() {
				return Err("Plugin is disabled".into());
			}
			let stored = read_stored(&directory.join("package.json"))?;
			if stored.package.manifest.id != id
				|| stored.package.manifest.kind != ExtensionKind::Plugin
			{
				return Err("Installed plugin identity changed".into());
			}
			let data = data_dir(root, &id, (!account.is_empty()).then_some(account.as_str()))?;
			migrate_data(&directory, &data)?;
			if is_proxy(&stored.package.manifest) != account.is_empty() {
				return Err("Plugin connection scope changed".into());
			}
			if invocation.selected_message.is_some()
				&& !stored.grants.contains(&Capability::SelectedMessage)
				|| invocation.composer.is_some() && !stored.grants.contains(&Capability::Composer)
				|| invocation.message_event.is_some()
					&& !stored.grants.contains(&Capability::MessageEvents)
				|| invocation.app_event.is_some() && !stored.grants.contains(&Capability::AppEvents)
				|| invocation.menu_target.is_some()
					&& !stored.grants.contains(&Capability::ContextMenus)
			{
				return Err("Plugin access was not granted".into());
			}
			if let Some(app) = &invocation.app {
				let mut granted = stored.package.manifest.clone();
				granted.capabilities.clone_from(&stored.grants);
				app.validate(&granted).map_err(|error| error.to_string())?;
			}
			invocation.storage = activation_storage(&stored, &data)?;
			if !trim_extended_input(&mut invocation, extensions::MAX_IO_BYTES) {
				return Err("Plugin input exceeds 256 KiB".into());
			}
			gate.check()?;
			let journal_action =
				stored.package.manifest.actions.iter().any(|action| {
					action.id == invocation.action && action.surface == Surface::Journal
				});
			let topic_action =
				stored.package.manifest.actions.iter().any(|action| {
					action.id == invocation.action && action.surface == Surface::Topic
				});
			if journal_action && !stored.grants.contains(&Capability::MessageJournal) {
				return Err("Plugin access was not granted".into());
			}
			let (services, pending) = SystemImports::for_plugin(&stored, Some(&data), gate);
			let mut output =
				extensions::invoke_with(&stored.package, &invocation, &stored.grants, services)
					.map_err(|e| e.to_string())?;
			gate.check()?;
			if output.api_proxy.is_some() && !stored.grants.contains(&Capability::ApiProxy) {
				return Err("API proxy access was not granted".into());
			}
			if output.rich_presence.is_some() && !stored.grants.contains(&Capability::RichPresence)
			{
				return Err("Rich presence access was not granted".into());
			}
			if output.interface_tweaks.is_some()
				&& !stored.grants.contains(&Capability::InterfaceTweaks)
			{
				return Err("Interface tweaks were not granted".into());
			}
			if output.interface_ui.is_some() && !stored.grants.contains(&Capability::InterfaceUi) {
				return Err("Interface replacement was not granted".into());
			}
			if output.start_screen.is_some() && !stored.grants.contains(&Capability::StartScreen) {
				return Err("Start screen customization was not granted".into());
			}
			let saved = output.storage.take();
			if saved.as_ref().is_some_and(|data| {
				!stored.grants.contains(&Capability::Storage) || data.len() > MAX_STORAGE
			}) {
				return Err("Plugin data exceeds its granted storage budget".into());
			}
			gate.check()?;
			let more = pending.journal_more();
			let topics_more = pending.topics_more();
			pending.commit()?;
			if let Some(saved) = saved {
				fs::create_dir_all(&data).map_err(|_| "Cannot create plugin data directory")?;
				atomic_write(&data.join("data.json"), saved.as_bytes(), gate)?;
			}
			if journal_action {
				return Ok(Event::JournalDelivered { id, more });
			}
			if topic_action {
				return Ok(Event::TopicDelivered {
					id,
					more: topics_more,
					captions: output.captions,
				});
			}
			Ok(Event::Invoked { id, output })
		}
		Job::Logout { account } => {
			if let Ok(scope) = data_scope(root, Some(&account)) {
				gate.archive.forget(&scope);
			}
			let directory = scope(root, ExtensionKind::Plugin, Some(&account))?;
			// The tombstone survives failed deletion and is retried before the next load.
			if directory.exists() {
				atomic_write(&directory.with_extension("disabled"), b"", gate)?;
				remove_owned_directory(&directory)?;
				remove_file(&directory.with_extension("disabled"))?;
			}
			Ok(Event::LoggedOut)
		}
	}
}

fn cached_official(root: &Path) -> Option<Catalog> {
	read_bounded(&root.join("catalog.json"), MAX_CATALOG)
		.ok()
		.and_then(|bytes| extensions::parse_catalog(&bytes).ok())
}

/// The built-in catalog with every added source's cached catalog.
fn merged_catalog(root: &Path, official: Option<Catalog>) -> Option<Catalog> {
	let sources = crate::sources::read(root)
		.into_iter()
		.filter_map(|source| Some((source.id.clone(), crate::sources::cached(root, &source.id)?)))
		.collect();
	crate::sources::merge(official, sources)
}

/// Each added source with its cached entry count and the error its refresh just hit.
fn source_statuses(
	root: &Path,
	errors: Vec<(String, String)>,
) -> Vec<crate::sources::SourceStatus> {
	crate::sources::read(root)
		.into_iter()
		.map(|source| crate::sources::SourceStatus {
			entries: crate::sources::cached(root, &source.id)
				.map_or(0, |catalog| catalog.entries.len()),
			error: errors
				.iter()
				.find(|(id, _)| *id == source.id)
				.map(|(_, error)| error.clone()),
			source,
		})
		.collect()
}

/// The source's saved token with where it may be sent.
fn source_auth(source: &crate::sources::CatalogSource) -> Option<crate::sources::Auth> {
	if !source.has_token {
		return None;
	}
	let token = secrets::store()
		.ok()?
		.load(&crate::sources::token_key(&source.id))
		.ok()??;
	Some(crate::sources::Auth {
		token,
		origins: source.token_scope(),
	})
}

/// Fetches the chosen sources' catalogs and caches the ones that parse; a failing source
/// keeps its last catalog. Returns each failure.
fn refresh_sources(
	root: &Path,
	gate: &Gate,
	chosen: impl Fn(&crate::sources::CatalogSource) -> bool,
) -> Vec<(String, String)> {
	let mut errors = Vec::new();
	for source in crate::sources::read(root)
		.into_iter()
		.filter(|source| chosen(source))
	{
		let auth = source_auth(&source);
		let fetched = download(
			&source.url,
			MAX_CATALOG,
			gate,
			Duration::from_secs(15),
			auth.as_ref(),
		)
		.and_then(|bytes| {
			extensions::parse_catalog(&bytes).map_err(|error| error.to_string())?;
			let path = crate::sources::cache_path(root, &source.id);
			fs::create_dir_all(path.parent().unwrap_or(root))
				.map_err(|_| "Cannot create catalog directory")?;
			atomic_write(&path, &bytes, gate)
		});
		if let Err(error) = fetched {
			errors.push((source.id, error));
		}
	}
	errors
}

fn write_sources(
	root: &Path,
	sources: &[crate::sources::CatalogSource],
	gate: &Gate,
) -> Result<(), String> {
	fs::create_dir_all(root).map_err(|_| "Cannot create catalog directory")?;
	let bytes = serde_json::to_vec(sources).map_err(|_| "Cannot encode catalog sources")?;
	atomic_write(&root.join(crate::sources::SOURCES_FILE), &bytes, gate)
}

fn cache_catalog(root: &Path, bytes: &[u8], gate: &Gate) -> Result<Catalog, String> {
	let catalog = extensions::parse_catalog(bytes).map_err(|error| error.to_string())?;
	fs::create_dir_all(root).map_err(|_| "Cannot create catalog directory")?;
	atomic_write(&root.join("catalog.json"), bytes, gate)?;
	Ok(catalog)
}

fn enable(
	root: &Path,
	source: InstallSource,
	grants: Vec<Capability>,
	account: Option<&str>,
	gate: &Gate,
) -> Result<InstalledExtension, String> {
	let (bytes, manifest, sha256, reviewed) = match source {
		InstallSource::Bundled {
			bytes,
			manifest,
			sha256,
		} => (bytes.to_vec(), manifest, sha256, true),
		InstallSource::Catalog(entry) => {
			// A source's package may need its token; the built-in catalog's never does.
			let auth = entry.catalog_source.as_deref().and_then(|id| {
				crate::sources::read(root)
					.into_iter()
					.find(|source| source.id == id)
					.and_then(|source| source_auth(&source))
			});
			let bytes = download(
				&entry.release_url,
				MAX_PACKAGE,
				gate,
				Duration::from_secs(60),
				auth.as_ref(),
			)?;
			if bytes.len() as u64 != entry.download_bytes {
				return Err("Extension download size changed".into());
			}
			(bytes, entry.manifest, entry.sha256, true)
		}
		InstallSource::Local {
			path,
			sha256,
			manifest,
		} => (read_bounded(&path, MAX_PACKAGE)?, manifest, sha256, false),
	};
	if !digest(&bytes).eq_ignore_ascii_case(&sha256) {
		return Err("Extension package checksum changed; inspect the release again".into());
	}
	let package = extensions::parse_package(&bytes).map_err(|e| e.to_string())?;
	if package.manifest != manifest {
		return Err("Package does not match the reviewed manifest".into());
	}
	if reviewed {
		let path = manifest_scope(root, &package.manifest, account)?
			.join(&package.manifest.id)
			.join("package.json");
		if path.exists() {
			let existing = read_stored(&path)?;
			if existing.local_theme || !existing.reviewed {
				return Err("A catalog update cannot replace a local or imported package".into());
			}
		}
	}
	let stored = Stored {
		local_theme: false,
		package,
		grants,
		reviewed,
		sha256: sha256.to_ascii_lowercase(),
		download_bytes: bytes.len() as u64,
	};
	install_stored(root, stored, account, gate)
}

fn encode_theme(package: &Package) -> Result<Vec<u8>, String> {
	if package.manifest.kind != ExtensionKind::Theme {
		return Err("Theme editor only accepts themes".into());
	}
	package.validate().map_err(|error| error.to_string())?;
	let bytes = serde_json::to_vec(package).map_err(|_| "Cannot encode theme package")?;
	if bytes.len() > MAX_PACKAGE {
		return Err("Theme package exceeds 16 MiB".into());
	}
	Ok(bytes)
}

fn install_stored(
	root: &Path,
	stored: Stored,
	account: Option<&str>,
	gate: &Gate,
) -> Result<InstalledExtension, String> {
	validate_grants(&stored.package.manifest, &stored.grants)?;
	let parent = manifest_scope(root, &stored.package.manifest, account)?;
	let directory = parent.join(&stored.package.manifest.id);
	if parent.with_extension("disabled").exists() {
		return Err("Account extension cleanup is incomplete; reopen Extensions to retry".into());
	}
	if !directory.exists() && count_directories(&parent)? >= MAX_PER_SCOPE {
		return Err(
			"Disable an extension first; each scope allows eight installed extensions".into(),
		);
	}
	if stored.package.manifest.kind == ExtensionKind::Plugin
		&& !is_proxy(&stored.package.manifest)
		&& !parent.exists()
		&& count_directories(&root.join("accounts"))? >= MAX_ACCOUNTS
	{
		return Err("Extension storage is full; log out an old account first".into());
	}
	let updating_theme = stored.reviewed
		&& stored.package.manifest.kind == ExtensionKind::Theme
		&& directory.join("package.json").exists();
	let activate = !updating_theme
		|| read_bounded(&parent.join("active.json"), 64)
			.is_ok_and(|id| id == stored.package.manifest.id.as_bytes());
	let data = (stored.package.manifest.kind == ExtensionKind::Plugin)
		.then(|| {
			data_dir(
				root,
				&stored.package.manifest.id,
				account.filter(|_| !is_proxy(&stored.package.manifest)),
			)
		})
		.transpose()?;
	if let Some(data) = &data {
		migrate_data(&directory, data)?;
	}
	let existed = data.as_ref().is_some_and(|data| data.exists());
	let mut summary = stored.summary(
		gate,
		match &data {
			Some(data) => activation_storage(&stored, data)?,
			None => None,
		},
		data.as_deref(),
		true,
	);
	summary.active_theme = stored.package.manifest.kind == ExtensionKind::Theme && activate;
	if !activate {
		summary.background_image = None;
	}
	if let Some(error) = &summary.error {
		// A first activation may have created the data directory for its database.
		if let Some(data) = data.as_ref().filter(|_| !existed) {
			remove_owned_directory(data)?;
		}
		return Err(error.clone());
	}
	let record = serde_json::to_vec(&stored).map_err(|_| "Cannot encode extension package")?;
	if record.len() > MAX_RECORD {
		return Err("Extension package exceeds storage budget".into());
	}
	gate.check()?;
	fs::create_dir_all(&directory).map_err(|_| "Cannot create extension directory")?;
	atomic_write(&directory.join("package.json"), &record, gate)?;
	remove_file(&directory.with_extension("disabled"))?;
	if stored.package.manifest.kind == ExtensionKind::Theme && activate {
		atomic_write(
			&parent.join("active.json"),
			stored.package.manifest.id.as_bytes(),
			gate,
		)?;
	}
	Ok(summary)
}

/// Approved subscriptions one account keeps.
const MAX_SUBSCRIPTIONS: usize = 64;
const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";

fn read_subscriptions(root: &Path, account: &str) -> Vec<Subscription> {
	data_scope(root, Some(account))
		.ok()
		.and_then(|scope| read_bounded(&scope.join(SUBSCRIPTIONS_FILE), 64 * 1024).ok())
		.and_then(|bytes| serde_json::from_slice::<Vec<Subscription>>(&bytes).ok())
		.unwrap_or_default()
		.into_iter()
		.take(MAX_SUBSCRIPTIONS)
		.collect()
}

fn write_subscriptions(
	root: &Path,
	account: &str,
	subscriptions: &[Subscription],
	gate: &Gate,
) -> Result<(), String> {
	let scope = data_scope(root, Some(account))?;
	fs::create_dir_all(&scope).map_err(|_| "Cannot create plugin data directory")?;
	let bytes = serde_json::to_vec(subscriptions).map_err(|_| "Cannot encode subscriptions")?;
	atomic_write(&scope.join(SUBSCRIPTIONS_FILE), &bytes, gate)
}

const HOOKS_FILE: &str = "hooks.json";

fn read_hooks(root: &Path, account: &str) -> Vec<ChannelHooks> {
	data_scope(root, Some(account))
		.ok()
		.and_then(|scope| read_bounded(&scope.join(HOOKS_FILE), 256 * 1024).ok())
		.and_then(|bytes| serde_json::from_slice::<Vec<ChannelHooks>>(&bytes).ok())
		.unwrap_or_default()
		.into_iter()
		.filter(|hooks| hooks.validate().is_ok() && !hooks.is_empty())
		.take(crate::hooks::MAX_CHANNELS)
		.collect()
}

fn write_hooks(
	root: &Path,
	account: &str,
	hooks: &[ChannelHooks],
	gate: &Gate,
) -> Result<(), String> {
	let scope = data_scope(root, Some(account))?;
	fs::create_dir_all(&scope).map_err(|_| "Cannot create plugin data directory")?;
	let bytes = serde_json::to_vec(hooks).map_err(|_| "Cannot encode message hooks")?;
	atomic_write(&scope.join(HOOKS_FILE), &bytes, gate)
}

/// Plugin data lives apart from packages, so disabling, updating, reinstalling or logging
/// out keeps it: `plugin-data/<account key | connection>/<id>`.
const DATA_ROOT: &str = "plugin-data";
/// Files a plugin's data consists of, moved out of package directories by older versions.
const DATA_FILES: [&str; 3] = ["data.json", database::FILE_NAME, "data.sqlite-journal"];
const SETTINGS_FILE: &str = "settings.json";
/// Plugin data directories listed for one scope.
const MAX_DATA_ENTRIES: usize = 256;

#[derive(Default, Serialize, Deserialize)]
struct DataSettings {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	database_bytes: Option<u64>,
	/// Secret IDs saved in the secret store, so deleting the data can forget them.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	secrets: Vec<String>,
	/// Folders and files the user picked for the plugin.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	handles: Vec<crate::files::FileHandle>,
}

fn read_settings(directory: &Path) -> DataSettings {
	read_bounded(&directory.join(SETTINGS_FILE), 64 * 1024)
		.ok()
		.and_then(|bytes| serde_json::from_slice(&bytes).ok())
		.unwrap_or_default()
}

fn write_settings(directory: &Path, settings: &DataSettings, gate: &Gate) -> Result<(), String> {
	fs::create_dir_all(directory).map_err(|_| "Cannot create plugin data directory")?;
	let bytes = serde_json::to_vec(settings).map_err(|_| "Cannot encode plugin data settings")?;
	atomic_write(&directory.join(SETTINGS_FILE), &bytes, gate)
}

/// The secret store key of `secret` for the plugin whose data directory is `directory`:
/// its scope directory's name (an account key or `connection`), plugin ID and secret ID.
fn secret_key(directory: &Path, secret: &str) -> Result<String, String> {
	let name = |path: Option<&Path>| {
		path.and_then(Path::file_name)
			.and_then(|name| name.to_str())
			.map(str::to_owned)
			.ok_or_else(|| "Invalid plugin data directory".to_string())
	};
	Ok(secrets::key(
		&name(directory.parent())?,
		&name(Some(directory))?,
		secret,
	))
}

/// Removes a plugin's data directory and forgets the secrets it recorded.
fn delete_data_dir(directory: &Path) -> Result<(), String> {
	let settings = read_settings(directory);
	if !settings.secrets.is_empty() {
		let store = secrets::store()?;
		for secret in &settings.secrets {
			store.forget(&secret_key(directory, secret)?)?;
		}
	}
	remove_owned_directory(directory)
}

fn data_scope(root: &Path, account: Option<&str>) -> Result<PathBuf, String> {
	Ok(root.join(DATA_ROOT).join(match account {
		Some(account) => account_key(account)?,
		None => "connection".into(),
	}))
}

/// A plugin's data directory; `account` is `None` for connection plugins.
pub fn data_dir(root: &Path, id: &str, account: Option<&str>) -> Result<PathBuf, String> {
	valid_id(id)?;
	Ok(data_scope(root, account)?.join(id))
}

/// Moves data an older version kept inside the package directory to the data directory.
fn migrate_data(package: &Path, data: &Path) -> Result<(), String> {
	for name in DATA_FILES {
		let old = package.join(name);
		if old.exists() && !data.join(name).exists() {
			fs::create_dir_all(data).map_err(|_| "Cannot create plugin data directory")?;
			fs::rename(&old, data.join(name)).map_err(|_| "Cannot move plugin data")?;
		}
	}
	Ok(())
}

fn database_quota(directory: &Path) -> u64 {
	read_settings(directory)
		.database_bytes
		.filter(|bytes| database::QUOTAS.contains(bytes))
		.unwrap_or(database::DEFAULT_QUOTA)
}

fn directory_bytes(directory: &Path) -> u64 {
	fs::read_dir(directory)
		.into_iter()
		.flatten()
		.take(64)
		.filter_map(|entry| entry.ok()?.metadata().ok())
		.filter(|metadata| metadata.is_file())
		.map(|metadata| metadata.len())
		.sum()
}

fn data_usage(root: &Path, account: Option<&str>) -> Result<Vec<PluginData>, String> {
	let mut scopes = vec![(data_scope(root, None)?, true)];
	if let Some(account) = account {
		scopes.push((data_scope(root, Some(account))?, false));
	}
	let mut usage = Vec::new();
	for (scope, connection) in scopes {
		let Ok(entries) = fs::read_dir(&scope) else {
			continue;
		};
		for entry in entries.take(MAX_DATA_ENTRIES).flatten() {
			let id = entry.file_name().to_string_lossy().into_owned();
			if !entry.file_type().is_ok_and(|kind| kind.is_dir()) || valid_id(&id).is_err() {
				continue;
			}
			let settings = read_settings(&entry.path());
			usage.push(PluginData {
				bytes: directory_bytes(&entry.path()).saturating_add(
					crate::files::folder_bytes(&entry.path().join(crate::files::PRIVATE_FOLDER))
						.unwrap_or(0),
				),
				database_quota: database_quota(&entry.path()),
				handles: settings
					.handles
					.iter()
					.map(crate::files::FileHandle::info)
					.collect(),
				secrets: settings.secrets,
				id,
				connection,
			});
		}
	}
	usage.sort_by(|a, b| a.id.cmp(&b.id));
	Ok(usage)
}

fn validate_grants(manifest: &Manifest, grants: &[Capability]) -> Result<(), String> {
	if grants.len() != manifest.capabilities.len()
		|| manifest
			.capabilities
			.iter()
			.any(|cap| !grants.contains(cap))
	{
		return Err("Confirm every requested capability before enabling this extension".into());
	}
	Ok(())
}

fn is_proxy(manifest: &Manifest) -> bool {
	manifest.capabilities.contains(&Capability::ApiProxy)
}
fn manifest_scope(
	root: &Path,
	manifest: &Manifest,
	account: Option<&str>,
) -> Result<PathBuf, String> {
	manifest.validate().map_err(|error| error.to_string())?;
	if is_proxy(manifest) {
		return Ok(root.join("proxy-plugins"));
	}
	if manifest.kind == ExtensionKind::Plugin && account.is_none() {
		return Err("Select an account before enabling plugins".into());
	}
	scope(root, manifest.kind, account)
}

fn scope(root: &Path, kind: ExtensionKind, account: Option<&str>) -> Result<PathBuf, String> {
	match kind {
		ExtensionKind::Theme => Ok(root.join("themes")),
		ExtensionKind::Plugin => match account {
			Some(account) => Ok(root.join("accounts").join(account_key(account)?)),
			None => Ok(root.join("proxy-plugins")),
		},
	}
}

pub fn account_key(account: &str) -> Result<String, String> {
	if account.is_empty() || account.len() > 128 {
		return Err("Invalid extension account scope".into());
	}
	Ok(digest(account.as_bytes()))
}

pub(crate) fn valid_id(id: &str) -> Result<(), String> {
	if !extensions::valid_id(id) {
		return Err("Invalid extension identifier".into());
	}
	Ok(())
}

/// Loads every installed theme and plugin for the account, retrying unfinished cleanup and
/// recording a per-extension error instead of failing the whole load.
fn load(
	root: &Path,
	account: Option<&str>,
	gate: &Gate,
) -> Result<Vec<InstalledExtension>, String> {
	cleanup(&root.join("accounts"), MAX_ACCOUNTS, gate)?;
	let mut scopes = vec![
		(root.join("themes"), ExtensionKind::Theme),
		(root.join("proxy-plugins"), ExtensionKind::Plugin),
	];
	if let Some(account) = account {
		scopes.push((
			scope(root, ExtensionKind::Plugin, Some(account))?,
			ExtensionKind::Plugin,
		));
	}
	let mut installed = Vec::new();
	for (parent, kind) in scopes {
		let cleanup_error = cleanup(&parent, MAX_PER_SCOPE, gate).err();
		let mut active_theme = None;
		if kind == ExtensionKind::Theme && parent.join("active.json").exists() {
			let active = String::from_utf8(read_bounded(&parent.join("active.json"), 64)?)
				.map_err(|_| "Invalid active theme")?;
			valid_id(&active)?;
			active_theme = Some(active);
		}
		if !parent.exists() {
			continue;
		}
		for entry in fs::read_dir(&parent)
			.map_err(|_| "Cannot read extensions")?
			.take(MAX_PER_SCOPE * 4 + 1)
		{
			let entry = entry.map_err(|_| "Cannot read extension entry")?;
			if entry
				.file_type()
				.map_err(|_| "Cannot read extension entry")?
				.is_dir()
			{
				if installed.len() >= MAX_PER_SCOPE * 3 {
					return Err("Too many installed extensions".into());
				}
				let id = entry.file_name().to_string_lossy().into_owned();
				valid_id(&id)?;
				let package_path = entry.path().join("package.json");
				if !package_path.exists() {
					remove_owned_directory(&entry.path())?;
					continue;
				}
				let stored = read_stored(&package_path).and_then(|stored| {
					if entry.path().with_extension("disabled").exists() {
						return Err(cleanup_error.clone().unwrap_or_else(|| {
							"Extension is disabled; cleanup needs retrying".into()
						}));
					}
					if stored.package.manifest.id != id
						|| stored.package.manifest.kind != kind
						|| is_proxy(&stored.package.manifest)
							!= (parent == root.join("proxy-plugins"))
					{
						return Err("Installed extension identity changed".into());
					}
					Ok(stored)
				});
				remove_file(&entry.path().join("package.partial"))?;
				let data = data_dir(
					root,
					&id,
					account.filter(|_| parent != root.join("proxy-plugins")),
				)?;
				if kind == ExtensionKind::Plugin {
					migrate_data(&entry.path(), &data)?;
					remove_file(&data.join("data.partial"))?;
				}
				installed.push(match stored {
					Ok(stored) => match activation_storage(&stored, &data) {
						Ok(storage) => {
							let mut summary = stored.summary(
								gate,
								storage,
								Some(&data),
								kind == ExtensionKind::Theme
									&& active_theme.as_deref() == Some(id.as_str()),
							);
							summary.active_theme = kind == ExtensionKind::Theme
								&& active_theme.as_deref() == Some(id.as_str());
							summary
						}
						Err(error) => {
							let mut summary = stored.summary(gate, None, Some(&data), false);
							summary.theme = None;
							summary.preserve_deleted_messages = false;
							summary.image_sharing = false;
							summary.rich_presence = None;
							summary.interface_tweaks.clear();
							summary.interface_ui = None;
							summary.start_screen = None;
							summary.error = Some(error);
							summary
						}
					},
					Err(error) => InstalledExtension {
						background_image: None,
						cover_image: None,
						local_theme: false,
						active_theme: false,
						manifest: Manifest {
							api_version: extensions::API_VERSION,
							id: id.clone(),
							name: id,
							version: "invalid".into(),
							author: "Unknown".into(),
							license: "Unknown".into(),
							source: "https://github.com/ViceVerse-cz/rustcord".into(),
							kind,
							capabilities: if parent == root.join("proxy-plugins") {
								vec![Capability::ApiProxy]
							} else {
								Vec::new()
							},
							actions: Vec::new(),
							network: Vec::new(),
							secrets: Vec::new(),
							broadcasts: Vec::new(),
							subscribes: Vec::new(),
						},
						theme: None,
						reviewed: false,
						sha256: String::new(),
						download_bytes: 0,
						error: Some(error),
						preserve_deleted_messages: false,
						image_sharing: false,
						rich_presence: None,
						api_proxy: None,
						interface_tweaks: Vec::new(),
						interface_ui: None,
						start_screen: None,
					},
				});
			}
		}
	}
	Ok(installed)
}

fn activation_storage(stored: &Stored, directory: &Path) -> Result<Option<String>, String> {
	let path = directory.join("data.json");
	if stored.grants.contains(&Capability::Storage) && path.exists() {
		String::from_utf8(read_bounded(&path, MAX_STORAGE)?)
			.map(Some)
			.map_err(|_| "Plugin data is invalid".into())
	} else {
		Ok(None)
	}
}

fn read_stored(path: &Path) -> Result<Stored, String> {
	let bytes = read_bounded(path, MAX_RECORD)?;
	let stored: Stored =
		serde_json::from_slice(&bytes).map_err(|_| "Installed extension metadata is invalid")?;
	if stored.sha256.len() != 64
		|| !stored.sha256.bytes().all(|b| b.is_ascii_hexdigit())
		|| stored.download_bytes == 0
		|| stored.download_bytes > MAX_PACKAGE as u64
	{
		return Err("Installed extension fingerprint is invalid".into());
	}
	stored.package.validate().map_err(|e| e.to_string())?;
	validate_grants(&stored.package.manifest, &stored.grants)?;
	Ok(stored)
}

fn disable(parent: &Path, id: &str, gate: &Gate) -> Result<(), String> {
	valid_id(id)?;
	if !parent.exists() {
		return Ok(());
	}
	let directory = parent.join(id);
	atomic_write(&directory.with_extension("disabled"), b"", gate)?;
	remove_owned_directory(&directory)?;
	if parent.join("active.json").exists()
		&& read_bounded(&parent.join("active.json"), 64)? == id.as_bytes()
	{
		remove_file(&parent.join("active.json"))?;
	}
	remove_file(&directory.with_extension("disabled"))
}

fn cleanup(parent: &Path, max: usize, gate: &Gate) -> Result<(), String> {
	if !parent.exists() {
		return Ok(());
	}
	for entry in fs::read_dir(parent)
		.map_err(|_| "Cannot inspect extension cleanup")?
		.take(max * 4 + 1)
	{
		gate.check()?;
		let entry = entry.map_err(|_| "Cannot inspect extension cleanup")?;
		if entry
			.path()
			.extension()
			.is_some_and(|extension| extension == "disabled")
		{
			let id = entry
				.path()
				.file_stem()
				.ok_or("Invalid cleanup marker")?
				.to_string_lossy()
				.into_owned();
			valid_id(&id)?;
			remove_owned_directory(&parent.join(&id))?;
			remove_file(&entry.path())?;
		} else if entry
			.path()
			.extension()
			.is_some_and(|extension| extension == "partial")
		{
			remove_file(&entry.path())?;
		}
	}
	Ok(())
}

fn count_directories(path: &Path) -> Result<usize, String> {
	if !path.exists() {
		return Ok(0);
	}
	let mut count = 0;
	for entry in fs::read_dir(path)
		.map_err(|_| "Cannot inspect extension budget")?
		.take(MAX_PER_SCOPE * 4 + 1)
	{
		if entry
			.map_err(|_| "Cannot inspect extension budget")?
			.file_type()
			.map_err(|_| "Cannot inspect extension budget")?
			.is_dir()
		{
			count += 1;
		}
	}
	Ok(count)
}

pub fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, String> {
	let file = File::open(path).map_err(|_| "Cannot read extension file")?;
	if !file
		.metadata()
		.map_err(|_| "Cannot inspect extension file")?
		.is_file()
	{
		return Err("Extension package must be a regular file".into());
	}
	let mut bytes = Vec::new();
	file.take(limit as u64 + 1)
		.read_to_end(&mut bytes)
		.map_err(|_| "Cannot read extension file")?;
	if bytes.len() > limit {
		return Err("Extension file exceeds its byte budget".into());
	}
	Ok(bytes)
}

fn export_theme(path: &Path, bytes: &[u8], gate: &Gate) -> Result<(), String> {
	gate.check()?;
	let mut nonce = [0; 16];
	getrandom::fill(&mut nonce).map_err(|_| "Cannot prepare theme export")?;
	let temporary = path.with_extension(format!("{}.partial", digest(&nonce)));
	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.open(&temporary)
		.map_err(|_| "Cannot create theme export")?;
	let result = (|| {
		file.write_all(bytes)
			.map_err(|_| "Cannot write theme export")?;
		file.sync_all().map_err(|_| "Cannot finish theme export")?;
		drop(file);
		gate.check()?;
		fs::rename(&temporary, path).map_err(|_| "Cannot replace theme export".to_owned())
	})();
	if result.is_err() {
		let _ = remove_file(&temporary);
	}
	result
}

fn atomic_write(path: &Path, bytes: &[u8], gate: &Gate) -> Result<(), String> {
	gate.check()?;
	let temporary = path.with_extension("partial");
	let result: Result<(), String> = (|| {
		let mut options = OpenOptions::new();
		options.write(true).create(true).truncate(true);
		#[cfg(unix)]
		{
			use std::os::unix::fs::OpenOptionsExt;
			options.mode(0o600);
		}
		let mut file = options
			.open(&temporary)
			.map_err(|_| "Cannot write extension data")?;
		file.write_all(bytes)
			.map_err(|_| "Cannot write extension data")?;
		file.sync_all()
			.map_err(|_| "Cannot finish extension data")?;
		drop(file);
		gate.check()?;
		fs::rename(&temporary, path).map_err(|_| "Cannot replace extension data".to_owned())
	})();
	if result.is_err() {
		let _ = remove_file(&temporary);
	}
	result
}

/// Atomically replaces, or with `None` removes, a device-level file beside the extension
/// directories, outside the job queue and its cancellation.
pub fn write_device_file(path: &Path, bytes: Option<&[u8]>) -> Result<(), String> {
	let Some(bytes) = bytes else {
		return remove_file(path);
	};
	if let Some(parent) = path.parent() {
		fs::create_dir_all(parent).map_err(|_| "Cannot create extension directory")?;
	}
	atomic_write(path, bytes, &Gate::detached())
}

fn remove_file(path: &Path) -> Result<(), String> {
	match fs::remove_file(path) {
		Ok(()) => Ok(()),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
		Err(_) => Err("Extension cleanup failed; retry or reopen Extensions".into()),
	}
}

fn remove_owned_directory(path: &Path) -> Result<(), String> {
	match fs::symlink_metadata(path) {
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
		Err(_) => return Err("Cannot inspect extension cleanup path".into()),
		Ok(metadata) if metadata.file_type().is_symlink() => {
			return Err("Extension directory cannot be a symbolic link".into());
		}
		Ok(_) => {}
	}
	fs::remove_dir_all(path).map_err(|_| "Extension cleanup failed; it will retry on launch".into())
}

fn load_preview(preview: &Preview, gate: &Gate) -> Option<egui::ColorImage> {
	gate.check().ok()?;
	preview.validate().ok()?;
	let bytes = if let Some(bytes) = cached_preview(&preview.sha256) {
		bytes
	} else {
		download(
			&preview.url,
			preview.download_bytes as usize,
			gate,
			Duration::from_secs(5),
			None,
		)
		.ok()?
	};
	gate.check().ok()?;
	let image = decode_preview(&bytes, preview)?;
	gate.check().ok()?;
	remember_preview(&preview.sha256, bytes);
	Some(image)
}

// Verified thumbnail bytes from this session: at most 32 previews or 4 MiB, whichever
// fills first. Scrolling the gallery re-decodes instead of re-downloading.
const MAX_CACHED_PREVIEWS: usize = 32;
const MAX_CACHED_PREVIEW_BYTES: usize = 4 * 1024 * 1024;
static PREVIEW_CACHE: std::sync::Mutex<VecDeque<(String, Vec<u8>)>> =
	std::sync::Mutex::new(VecDeque::new());

fn cached_preview(sha256: &str) -> Option<Vec<u8>> {
	let mut cache = PREVIEW_CACHE.lock().ok()?;
	let index = cache
		.iter()
		.position(|(hash, _)| hash.eq_ignore_ascii_case(sha256))?;
	// Most recently used previews move to the back so eviction drops stale ones first.
	let entry = cache.remove(index)?;
	let bytes = entry.1.clone();
	cache.push_back(entry);
	Some(bytes)
}

fn remember_preview(sha256: &str, bytes: Vec<u8>) {
	if bytes.is_empty() || bytes.len() > extensions::MAX_PREVIEW_BYTES {
		return;
	}
	let Ok(mut cache) = PREVIEW_CACHE.lock() else {
		return;
	};
	cache.retain(|(hash, _)| !hash.eq_ignore_ascii_case(sha256));
	cache.push_back((sha256.to_ascii_lowercase(), bytes));
	while cache.len() > MAX_CACHED_PREVIEWS
		|| cache.iter().map(|(_, bytes)| bytes.len()).sum::<usize>() > MAX_CACHED_PREVIEW_BYTES
	{
		cache.pop_front();
	}
}

pub fn read_background(path: &Path) -> Result<(Vec<u8>, egui::ColorImage), String> {
	let bytes = read_bounded(path, extensions::MAX_BACKGROUND_BYTES)?;
	let image = decode_background(&bytes)?;
	Ok((bytes, image))
}

pub fn read_cover(path: &Path) -> Result<(Vec<u8>, egui::ColorImage), String> {
	let bytes = read_bounded(path, extensions::MAX_BACKGROUND_BYTES)?;
	let image = decode_cover(&bytes)?;
	Ok((bytes, image))
}

fn decode_cover(bytes: &[u8]) -> Result<egui::ColorImage, String> {
	if bytes.len() > extensions::MAX_BACKGROUND_BYTES {
		return Err("Cover image exceeds 2 MiB".into());
	}
	let image = decode_static_image(bytes, 4_000_000)
		.ok_or("Choose a static PNG or JPEG within 4096 pixels per edge and 4 million pixels")?
		.thumbnail(640, 360)
		.into_rgba8();
	Ok(egui::ColorImage::from_rgba_unmultiplied(
		[image.width() as usize, image.height() as usize],
		image.as_raw(),
	))
}

fn package_cover(package: &Package) -> Result<Option<Arc<egui::ColorImage>>, String> {
	if package.cover_image.is_empty() {
		Ok(None)
	} else {
		decode_cover(&package.cover_image).map(|image| Some(Arc::new(image)))
	}
}

pub fn decode_background(bytes: &[u8]) -> Result<egui::ColorImage, String> {
	if bytes.len() > extensions::MAX_BACKGROUND_BYTES {
		return Err("Background image exceeds 2 MiB".into());
	}
	let image = decode_static_image(bytes, 4_000_000)
		.ok_or("Choose a static PNG or JPEG within 4096 pixels per edge and 4 million pixels")?
		.into_rgba8();
	Ok(egui::ColorImage::from_rgba_unmultiplied(
		[image.width() as usize, image.height() as usize],
		image.as_raw(),
	))
}

fn package_background(package: &Package) -> Result<Option<Arc<egui::ColorImage>>, String> {
	if package.background_image.is_empty() {
		Ok(None)
	} else {
		decode_background(&package.background_image).map(|image| Some(Arc::new(image)))
	}
}

fn decode_static_image(bytes: &[u8], max_pixels: u64) -> Option<image::DynamicImage> {
	use image::ImageDecoder;
	let format = image::guess_format(bytes).ok()?;
	if !matches!(format, image::ImageFormat::Png | image::ImageFormat::Jpeg) {
		return None;
	}
	let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
	let mut limits = image::Limits::default();
	limits.max_image_width = Some(4096);
	limits.max_image_height = Some(4096);
	limits.max_alloc = Some(32 * 1024 * 1024);
	reader.limits(limits);
	let decoder = reader.into_decoder().ok()?;
	let (width, height) = decoder.dimensions();
	if width == 0
		|| height == 0
		|| width > 4096
		|| height > 4096
		|| u64::from(width) * u64::from(height) > max_pixels
		|| decoder.total_bytes() > 32 * 1024 * 1024
	{
		return None;
	}
	image::DynamicImage::from_decoder(decoder).ok()
}

fn decode_preview(bytes: &[u8], preview: &Preview) -> Option<egui::ColorImage> {
	if bytes.len() > extensions::MAX_PREVIEW_BYTES
		|| bytes.len() as u64 != preview.download_bytes
		|| !digest(bytes).eq_ignore_ascii_case(&preview.sha256)
	{
		return None;
	}
	let mut image = decode_static_image(bytes, 4 * 1024 * 1024)?;
	if image.width() > 640 || image.height() > 360 {
		image = image.thumbnail(640, 360);
	}
	let image = image.into_rgba8();
	Some(egui::ColorImage::from_rgba_unmultiplied(
		[image.width() as usize, image.height() as usize],
		image.as_raw(),
	))
}

fn digest(bytes: &[u8]) -> String {
	format!("{:x}", Sha256::digest(bytes))
}

fn download(
	raw: &str,
	limit: usize,
	gate: &Gate,
	timeout: Duration,
	auth: Option<&crate::sources::Auth>,
) -> Result<Vec<u8>, String> {
	tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.build()
		.map_err(|_| "Extension downloader unavailable")?
		.block_on(async {
			tokio::select! {
				biased;
				_ = gate.wake_cancel.notified() => Err("Extension operation cancelled".into()),
				result = tokio::time::timeout(timeout, download_https(raw, limit, gate, auth)) => result.map_err(|_| "Extension download timed out")?,
			}
		})
}

async fn download_https(
	raw: &str,
	limit: usize,
	gate: &Gate,
	auth: Option<&crate::sources::Auth>,
) -> Result<Vec<u8>, String> {
	let mut url = url::Url::parse(raw).map_err(|_| "Invalid extension download URL")?;
	for _ in 0..6 {
		gate.check()?;
		let host = url.host_str().ok_or("Invalid extension download host")?;
		if url.scheme() != "https"
			|| !url.username().is_empty()
			|| url.password().is_some()
			|| url.port_or_known_default() != Some(443)
			|| !host.contains('.')
		{
			return Err("Extensions require credential-free public HTTPS URLs".into());
		}
		let addresses: Vec<_> = tokio::net::lookup_host((host, 443))
			.await
			.map_err(|_| "Extension download DNS failed")?
			.take(16)
			.collect();
		if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
			return Err("Extension downloads cannot access private networks".into());
		}
		// Pin the validated DNS result; redirects receive a fresh validation and client.
		let client = wreq::Client::builder()
			.no_proxy()
			.no_gzip()
			.redirect(wreq::redirect::Policy::none())
			.resolve_to_addrs(host.to_owned(), addresses.iter().copied())
			.timeout(Duration::from_secs(30))
			.build()
			.map_err(|_| "Extension downloader unavailable")?;
		let mut request = client
			.get(url.as_str())
			.header(wreq::header::ACCEPT_ENCODING, "identity");
		// GitHub's API returns file contents and release assets raw only when asked, and
		// refuses requests without a client name.
		if host == "api.github.com" {
			request = request.header(wreq::header::USER_AGENT, "Serein");
			if let Some(accept) = github_accept(url.path()) {
				request = request.header(wreq::header::ACCEPT, accept);
			}
		}
		// Each hop decides on its own, so a redirect to another origin never carries it.
		if let Some(token) = auth.and_then(|auth| auth.for_url(&url)) {
			request = request.header(wreq::header::AUTHORIZATION, format!("Bearer {token}"));
		}
		let response = request
			.send()
			.await
			.map_err(|_| "Extension download failed")?;
		if response.status().is_redirection() {
			let location = response
				.headers()
				.get(wreq::header::LOCATION)
				.and_then(|v| v.to_str().ok())
				.ok_or("Invalid extension redirect")?;
			url = url
				.join(location)
				.map_err(|_| "Invalid extension redirect")?;
			continue;
		}
		if response.status() != wreq::StatusCode::OK {
			return Err(
				"Extension download unavailable; refresh the catalog or use a local package".into(),
			);
		}
		if response
			.content_length()
			.is_some_and(|size| size > limit as u64)
			|| response
				.headers()
				.get(wreq::header::CONTENT_ENCODING)
				.is_some_and(|encoding| encoding != "identity")
		{
			return Err("Extension download size or encoding is unsupported".into());
		}
		let mut bytes = Vec::new();
		let mut response_body = std::pin::pin!(response.bytes_stream());
		while let Some(chunk) = futures_util::StreamExt::next(&mut response_body)
			.await
			.transpose()
			.map_err(|_| "Extension download interrupted")?
		{
			gate.check()?;
			if bytes.len().saturating_add(chunk.len()) > limit {
				return Err("Extension download exceeds its byte budget".into());
			}
			bytes.extend_from_slice(&chunk);
		}
		return Ok(bytes);
	}
	Err("Too many extension download redirects".into())
}

fn github_accept(path: &str) -> Option<&'static str> {
	if path.contains("/releases/assets/") {
		Some("application/octet-stream")
	} else if path.contains("/contents/") {
		Some("application/vnd.github.raw")
	} else {
		None
	}
}

pub(crate) fn public_ip(ip: IpAddr) -> bool {
	match ip {
		IpAddr::V4(ip) => {
			let [a, b, _, _] = ip.octets();
			!ip.is_private()
				&& !ip.is_loopback()
				&& !ip.is_link_local()
				&& !ip.is_broadcast()
				&& !ip.is_documentation()
				&& !ip.is_multicast()
				&& a != 0 && a < 240
				&& !(a == 100 && (64..=127).contains(&b))
				&& !(a == 198 && (b == 18 || b == 19))
				&& !(a == 192 && b == 0)
				&& ip != Ipv4Addr::UNSPECIFIED
		}
		IpAddr::V6(ip) => {
			let segments = ip.segments();
			segments[0] & 0xe000 == 0x2000
				&& !(segments[0] == 0x2001 && (segments[1] == 0xdb8 || segments[1] < 0x200))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	fn no_wake() -> Wake {
		Arc::new(|| {})
	}

	#[test]
	fn extended_input_is_trimmed_after_storage_is_loaded() {
		let mut invocation = Invocation {
			action: "run".into(),
			storage: Some("stored state".into()),
			queries: Some(Box::default()),
			..Default::default()
		};
		let mut without_extended = invocation.clone();
		without_extended.queries = None;
		let limit = serde_json::to_vec(&without_extended).unwrap().len();
		assert!(trim_extended_input(&mut invocation, limit));
		assert!(invocation.queries.is_none());
		assert_eq!(invocation.storage.as_deref(), Some("stored state"));
	}

	struct Profile(PathBuf);
	impl Profile {
		fn new() -> Self {
			let mut nonce = [0; 16];
			getrandom::fill(&mut nonce).unwrap();
			let path =
				std::env::temp_dir().join(format!("serein-extension-test-{}", digest(&nonce)));
			fs::create_dir(&path).unwrap();
			Self(path)
		}
	}
	impl Drop for Profile {
		fn drop(&mut self) {
			fs::remove_dir_all(&self.0).unwrap();
		}
	}
	fn gate() -> Gate {
		Gate::detached()
	}

	fn theme(id: &str) -> Package {
		Package {
			manifest: Manifest {
				api_version: 1,
				id: id.into(),
				name: id.into(),
				version: "1.0.0".into(),
				author: "Synthetic test".into(),
				license: "MIT".into(),
				source: "https://github.com/ViceVerse-cz/rustcord".into(),
				kind: ExtensionKind::Theme,
				network: Vec::new(),
				secrets: Vec::new(),
				broadcasts: Vec::new(),
				subscribes: Vec::new(),
				capabilities: Vec::new(),
				actions: Vec::new(),
			},
			theme: Some(Theme::default()),
			background_image: Vec::new(),
			cover_image: Vec::new(),
			wasm: Vec::new(),
		}
	}
	fn source(profile: &Profile, package: &Package) -> InstallSource {
		let bytes = serde_json::to_vec(package).unwrap();
		let path = profile.0.join(format!("{}.json", package.manifest.id));
		fs::write(&path, &bytes).unwrap();
		InstallSource::Local {
			path,
			sha256: digest(&bytes),
			manifest: package.manifest.clone(),
		}
	}
	fn message_event_job() -> Job {
		Job::Invoke {
			id: "message-counter".into(),
			account: "account".into(),
			invocation: Invocation {
				action: "message-event".into(),
				message_event: Some(Box::new(extensions::MessageEvent {
					kind: extensions::MessageEventKind::Create,
					channel_id: "1".into(),
					message_id: "2".into(),
					author_id: Some("3".into()),
					content: Some("Synthetic message".into()),
				})),
				..Default::default()
			},
		}
	}

	#[test]
	fn proxy_plugins_are_prelogin_global_and_survive_account_logout() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let mut package = extensions::parse_package(include_bytes!(
			"../../../examples/extensions/packages/message-counter.serein-extension"
		))
		.unwrap();
		assert!(manifest_scope(&root, &package.manifest, None).is_err());
		package.manifest.id = "api-proxy".into();
		package.manifest.capabilities = vec![Capability::ApiProxy, Capability::Storage];
		package.manifest.actions = vec![extensions::Action {
			id: "settings".into(),
			label: "Settings".into(),
			surface: Surface::Panel,
		}];
		let installed = enable(
			&root,
			source(&profile, &package),
			package.manifest.capabilities.clone(),
			None,
			&gate(),
		)
		.unwrap();
		assert!(installed.error.is_none());
		let directory = root.join("proxy-plugins/api-proxy");
		assert!(directory.join("package.json").exists());
		for account in [None, Some("one"), Some("two")] {
			assert!(
				load(&root, account, &gate())
					.unwrap()
					.iter()
					.any(|entry| entry.manifest.id == "api-proxy")
			);
		}
		assert!(
			run(
				&root,
				Job::Invoke {
					id: "api-proxy".into(),
					account: "one".into(),
					invocation: Invocation {
						action: "settings".into(),
						..Default::default()
					}
				},
				&gate()
			)
			.is_err()
		);
		run(
			&root,
			Job::Logout {
				account: "one".into(),
			},
			&gate(),
		)
		.unwrap();
		assert!(directory.exists());
		run(
			&root,
			Job::Disable {
				id: "api-proxy".into(),
				kind: ExtensionKind::Plugin,
				account: None,
				delete_data: true,
			},
			&gate(),
		)
		.unwrap();
		assert!(!directory.exists());
	}

	#[test]
	fn bundled_themes_open_without_installing_and_preserve_preview_intent() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		for starter in starters().unwrap() {
			let InstallSource::Bundled { manifest, .. } = starter.source else {
				unreachable!()
			};
			let result = run(
				&root,
				Job::EditTheme {
					id: manifest.id.clone(),
					preview: true,
				},
				&gate(),
			);
			if manifest.kind == ExtensionKind::Theme {
				let Event::EditTheme {
					package,
					local_theme,
					preview,
					..
				} = result.unwrap()
				else {
					panic!("expected theme")
				};
				assert_eq!(package.manifest.id, manifest.id);
				assert!(package.theme.is_some());
				assert!(preview && !local_theme);
			} else {
				assert!(result.is_err());
			}
		}
		assert!(!root.exists(), "preview must not install a theme");
		let original = extensions::parse_package(include_bytes!(
			"../../../extensions/themes/ocean.serein-extension"
		))
		.unwrap();
		let directory = root.join("themes").join(&original.manifest.id);
		fs::create_dir_all(&directory).unwrap();
		fs::write(directory.join("package.json"), b"invalid").unwrap();
		assert!(
			run(
				&root,
				Job::EditTheme {
					id: original.manifest.id,
					preview: false
				},
				&gate()
			)
			.is_err(),
			"do not hide a corrupt installed package behind the bundled original"
		);
	}

	#[test]
	fn message_event_jobs_validate_fields_and_count_payload_bytes_before_queueing() {
		let profile = Profile::new();
		let mut host = ExtensionHost::new(profile.0.join("extensions"));
		for invalid_id in [true, false] {
			let mut job = message_event_job();
			let Job::Invoke { invocation, .. } = &mut job else {
				unreachable!()
			};
			if invalid_id {
				invocation.message_event.as_mut().unwrap().channel_id = "invalid".into();
			} else {
				invocation.storage =
					Some("x".repeat(extensions::MAX_IO_BYTES - invocation.action.len()));
			}
			assert!(host.submit(job, &no_wake()).is_err());
			assert!(!host.busy());
		}
		assert!(!host.root.exists());
	}

	#[test]
	fn message_event_worker_requires_grants_serializes_storage_and_rejects_cancelled_jobs() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let package = extensions::parse_package(include_bytes!(
			"../../../examples/extensions/packages/message-counter.serein-extension"
		))
		.unwrap();
		enable(
			&root,
			source(&profile, &package),
			package.manifest.capabilities.clone(),
			Some("account"),
			&gate(),
		)
		.unwrap();
		let directory = scope(&root, ExtensionKind::Plugin, Some("account"))
			.unwrap()
			.join(&package.manifest.id);
		let package_path = directory.join("package.json");
		let data_path = data_dir(&root, &package.manifest.id, Some("account")).unwrap();
		let mut stored = read_stored(&package_path).unwrap();
		stored
			.grants
			.retain(|grant| *grant != Capability::MessageEvents);
		fs::write(&package_path, serde_json::to_vec(&stored).unwrap()).unwrap();
		assert!(run(&root, message_event_job(), &gate()).is_err());
		assert!(!data_path.join("data.json").exists());
		stored.grants.push(Capability::MessageEvents);
		fs::write(&package_path, serde_json::to_vec(&stored).unwrap()).unwrap();

		let mut host = ExtensionHost::new(root.clone());
		let context = no_wake();
		let first = host.submit(message_event_job(), &context).unwrap();
		let mut second_job = message_event_job();
		let Job::Invoke { invocation, .. } = &mut second_job else {
			unreachable!()
		};
		// The worker must reload committed storage rather than use the queued snapshot.
		invocation.storage = Some(r#"{"create":99,"update":0,"delete":0}"#.into());
		let second = host.submit(second_job, &context).unwrap();
		let deadline = std::time::Instant::now() + Duration::from_secs(10);
		let mut completed = Vec::new();
		while host.busy() {
			if let Some((token, result)) = host.poll() {
				let Event::Invoked { output, .. } = result.unwrap() else {
					panic!("expected message event result")
				};
				assert!(output.storage.is_none() && output.panel.is_empty());
				completed.push(token);
			}
			assert!(
				std::time::Instant::now() < deadline,
				"extension worker timed out"
			);
			std::thread::sleep(Duration::from_millis(1));
		}
		assert_eq!(completed, [first, second]);
		let data = fs::read(data_path.join("data.json")).unwrap();
		assert_eq!(
			serde_json::from_slice::<serde_json::Value>(&data).unwrap(),
			serde_json::json!({ "create": 2, "update": 0, "delete": 0 })
		);
		let cancelled = gate();
		cancelled.generation.fetch_add(1, Ordering::Release);
		assert!(run(&root, message_event_job(), &cancelled).is_err());
		assert_eq!(fs::read(data_path.join("data.json")).unwrap(), data);
		assert!(!data_path.join("data.partial").exists());
	}

	/// A panel plugin that creates a table, inserts a row and returns `output`.
	fn database_plugin(output: &str) -> Package {
		fn data(at: u32, text: &str) -> String {
			let escaped: String = text.bytes().map(|b| format!("\\{b:02x}")).collect();
			format!(r#"(data (i32.const {at}) "{escaped}")"#)
		}
		let create = r#"{"sql":"CREATE TABLE IF NOT EXISTS hits (n)"}"#;
		let insert = r#"{"sql":"INSERT INTO hits VALUES (?)","params":[1]}"#;
		let wasm = wat::parse_str(format!(
			r#"(module
			(import "serein" "db_execute" (func $db (param i32 i32) (result i32)))
			(memory (export "memory") 1)
			{} {} {}
			(func (export "serein_alloc") (param i32) (result i32) (i32.const 8192))
			(func (export "serein_invoke") (param i32 i32) (result i64)
				(drop (call $db (i32.const 1024) (i32.const {})))
				(drop (call $db (i32.const 2048) (i32.const {})))
				(i64.const {})))"#,
			data(1024, create),
			data(2048, insert),
			data(4096, output),
			create.len(),
			insert.len(),
			(4096_u64 << 32) | output.len() as u64,
		))
		.unwrap();
		Package {
			manifest: Manifest {
				api_version: extensions::API_VERSION,
				id: "database-plugin".into(),
				name: "Database plugin".into(),
				version: "1.0.0".into(),
				author: "Synthetic test".into(),
				license: "MIT".into(),
				source: "https://github.com/example/plugin".into(),
				kind: ExtensionKind::Plugin,
				network: Vec::new(),
				secrets: Vec::new(),
				broadcasts: Vec::new(),
				subscribes: Vec::new(),
				capabilities: vec![Capability::Database],
				actions: vec![extensions::Action {
					id: "run".into(),
					label: "Run".into(),
					surface: Surface::Panel,
				}],
			},
			theme: None,
			background_image: Vec::new(),
			cover_image: Vec::new(),
			wasm,
		}
	}

	#[test]
	fn plugin_data_commits_accepted_invocations_and_outlives_disable_and_logout_unless_deleted() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let package = database_plugin("{}");
		let install = |package: &Package, grants: Vec<Capability>| {
			enable(
				&root,
				source(&profile, package),
				grants,
				Some("account"),
				&gate(),
			)
		};
		let invoke = || {
			run(
				&root,
				Job::Invoke {
					id: "database-plugin".into(),
					account: "account".into(),
					invocation: Invocation {
						action: "run".into(),
						..Default::default()
					},
				},
				&gate(),
			)
		};
		let directory = scope(&root, ExtensionKind::Plugin, Some("account"))
			.unwrap()
			.join("database-plugin");
		let data = data_dir(&root, "database-plugin", Some("account")).unwrap();
		let rows = || {
			let mut database = crate::database::PluginDatabase::new(
				data.join(crate::database::FILE_NAME),
				crate::database::DEFAULT_QUOTA,
			);
			match database.execute(br#"{"sql":"SELECT count(*) FROM hits"}"#) {
				extensions::Reply::Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
					.unwrap()["rows"][0][0]
					.as_i64()
					.unwrap(),
				_ => 0,
			}
		};
		// Without the grant the import is refused before any SQL runs.
		install(&package, vec![Capability::Database]).unwrap();
		let package_path = directory.join("package.json");
		let mut stored = read_stored(&package_path).unwrap();
		stored.grants.clear();
		fs::write(&package_path, serde_json::to_vec(&stored).unwrap()).unwrap();
		assert!(invoke().is_err());
		assert!(!data.join(crate::database::FILE_NAME).exists());
		install(&package, vec![Capability::Database]).unwrap();
		invoke().unwrap();
		invoke().unwrap();
		assert_eq!(rows(), 2);
		// Output the host rejects (storage without its grant) discards that invocation's rows.
		let rejected = database_plugin(r#"{"storage":"x"}"#);
		install(&rejected, vec![Capability::Database]).unwrap();
		assert!(invoke().is_err());
		assert_eq!(rows(), 2);
		// Updating, disabling with "keep data", logging out and reinstalling all keep it.
		install(&package, vec![Capability::Database]).unwrap();
		assert_eq!(rows(), 2);
		let disable = |delete_data| {
			run(
				&root,
				Job::Disable {
					id: "database-plugin".into(),
					kind: ExtensionKind::Plugin,
					account: Some("account".into()),
					delete_data,
				},
				&gate(),
			)
			.unwrap();
		};
		disable(false);
		assert!(!directory.exists());
		let Event::DataUsage(usage) = run(
			&root,
			Job::DataUsage {
				account: Some("account".into()),
			},
			&gate(),
		)
		.unwrap() else {
			panic!("expected data usage")
		};
		assert!(
			matches!(&usage[..], [PluginData { id, connection: false, bytes, database_quota, .. }]
			if id == "database-plugin" && *bytes > 0 && *database_quota == crate::database::DEFAULT_QUOTA)
		);
		install(&package, vec![Capability::Database]).unwrap();
		run(
			&root,
			Job::Logout {
				account: "account".into(),
			},
			&gate(),
		)
		.unwrap();
		assert!(!directory.exists());
		install(&package, vec![Capability::Database]).unwrap();
		invoke().unwrap();
		assert_eq!(rows(), 3);
		// A chosen quota persists with the data; only offered sizes are accepted.
		let quota = |bytes| {
			run(
				&root,
				Job::SetDatabaseQuota {
					id: "database-plugin".into(),
					account: Some("account".into()),
					connection: false,
					bytes,
				},
				&gate(),
			)
		};
		let gib = 1024 * 1024 * 1024;
		assert!(
			matches!(quota(gib), Ok(Event::DataUsage(usage)) if usage[0].database_quota == gib)
		);
		assert!(
			validate_job(&Job::SetDatabaseQuota {
				id: "database-plugin".into(),
				account: None,
				connection: false,
				bytes: 5,
			})
			.is_err()
		);
		// Deleting on disable removes the data too.
		disable(true);
		assert!(!data.exists());
	}

	#[test]
	fn secrets_live_in_the_store_and_are_forgotten_with_the_data() {
		secrets::install(Arc::new(secrets::MemoryStore::default()));
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let mut package = database_plugin("{}");
		package.manifest.id = "secret-plugin".into();
		package.manifest.capabilities = vec![Capability::Network];
		package.manifest.network = vec!["https://api.example.org".into()];
		package.manifest.secrets = vec![extensions::network::SecretSpec {
			id: "api_key".into(),
			label: "API key".into(),
			origins: vec!["https://api.example.org".into()],
		}];
		package.wasm = wat::parse_str(
			r#"(module (memory (export "memory") 1)
			(func (export "serein_alloc") (param i32) (result i32) (i32.const 0))
			(func (export "serein_invoke") (param i32 i32) (result i64) (i64.const 0)))"#,
		)
		.unwrap();
		enable(
			&root,
			source(&profile, &package),
			vec![Capability::Network],
			Some("account"),
			&gate(),
		)
		.unwrap();
		let set = |secret: &str, value: Option<&str>| {
			run(
				&root,
				Job::SetSecret {
					id: "secret-plugin".into(),
					account: Some("account".into()),
					connection: false,
					secret: secret.into(),
					value: value.map(|value| zeroize::Zeroizing::new(value.into())),
				},
				&gate(),
			)
		};
		let data = data_dir(&root, "secret-plugin", Some("account")).unwrap();
		let key = secret_key(&data, "api_key").unwrap();
		let Ok(Event::DataUsage(usage)) = set("api_key", Some("value-1")) else {
			panic!("secret not saved")
		};
		assert_eq!(usage[0].secrets, ["api_key"]);
		assert_eq!(
			secrets::store()
				.unwrap()
				.load(&key)
				.unwrap()
				.as_deref()
				.map(String::as_str),
			Some("value-1")
		);
		// The value is never written to the data directory.
		for file in fs::read_dir(&data).unwrap() {
			let bytes = fs::read(file.unwrap().path()).unwrap();
			assert!(!String::from_utf8_lossy(&bytes).contains("value-1"));
		}
		assert!(set("undeclared", Some("x")).is_err());
		assert!(
			validate_job(&Job::SetSecret {
				id: "secret-plugin".into(),
				account: None,
				connection: false,
				secret: "api_key".into(),
				value: Some(zeroize::Zeroizing::new("line\nbreak".into())),
			})
			.is_err()
		);
		// Keeping the data keeps the secret; deleting the data forgets it.
		for delete_data in [false, true] {
			run(
				&root,
				Job::Disable {
					id: "secret-plugin".into(),
					kind: ExtensionKind::Plugin,
					account: Some("account".into()),
					delete_data,
				},
				&gate(),
			)
			.unwrap();
			assert_eq!(
				secrets::store().unwrap().load(&key).unwrap().is_some(),
				!delete_data
			);
			if !delete_data {
				enable(
					&root,
					source(&profile, &package),
					vec![Capability::Network],
					Some("account"),
					&gate(),
				)
				.unwrap();
			}
		}
	}

	#[test]
	fn channel_hooks_need_the_plugins_hook_and_disabling_switches_them_off() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let gate = gate();
		let mut hooker = topic_plugin("hooker", true, "", "", "");
		hooker.manifest.broadcasts.clear();
		hooker.manifest.capabilities = vec![Capability::SendHook, Capability::MessageAnnotations];
		hooker.manifest.actions[0].surface = Surface::Send;
		hooker.manifest.actions.push(extensions::Action {
			id: "notes".into(),
			label: "Notes".into(),
			surface: Surface::Annotate,
		});
		let plain = topic_plugin("plain", true, "", "", "");
		let grants = hooker.manifest.capabilities.clone();
		enable(
			&root,
			source(&profile, &hooker),
			grants,
			Some("account"),
			&gate,
		)
		.unwrap();
		enable(
			&root,
			source(&profile, &plain),
			Vec::new(),
			Some("account"),
			&gate,
		)
		.unwrap();
		let set = |send: &str, annotate: &[&str]| {
			run(
				&root,
				Job::SetChannelHooks {
					account: "account".into(),
					hooks: ChannelHooks {
						channel: model::Id(20),
						send: Some(crate::hooks::SendHook {
							plugin: send.into(),
							mode: crate::hooks::SendMode::Auto,
							fallback: crate::hooks::Fallback::SendOriginal,
						}),
						annotate: annotate.iter().map(|id| id.to_string()).collect(),
					},
				},
				&gate,
			)
		};
		// Only a plugin with that hook can be switched on.
		assert!(set("plain", &[]).is_err());
		assert!(set("hooker", &["plain"]).is_err());
		let Ok(Event::ChannelHooks(saved)) = set("hooker", &["hooker"]) else {
			panic!("hooks not saved")
		};
		assert_eq!(saved.len(), 1);
		let Ok(Event::Loaded { hooks, .. }) = run(
			&root,
			Job::Load {
				account: Some("account".into()),
			},
			&gate,
		) else {
			panic!("not loaded")
		};
		assert_eq!(hooks, saved, "hooks survive a restart");
		run(
			&root,
			Job::Disable {
				id: "hooker".into(),
				kind: ExtensionKind::Plugin,
				account: Some("account".into()),
				delete_data: false,
			},
			&gate,
		)
		.unwrap();
		assert!(read_hooks(&root, "account").is_empty());
	}

	/// A plugin `id` whose one action runs `body`; with `broadcasts` it is a panel action,
	/// otherwise a `topic` action subscribed to `serein.test/1`.
	fn topic_plugin(id: &str, broadcasts: bool, imports: &str, data: &str, body: &str) -> Package {
		let mut package = database_plugin("{}");
		package.manifest.id = id.into();
		package.manifest.capabilities.clear();
		if broadcasts {
			package.manifest.broadcasts = vec!["serein.test/1".into()];
			package.manifest.actions[0].surface = Surface::Panel;
		} else {
			package.manifest.subscribes = vec!["serein.test/1".into()];
			package.manifest.actions[0].surface = Surface::Topic;
		}
		package.wasm = wat::parse_str(format!(
			r#"(module {imports} (memory (export "memory") 1) {data}
			(data (i32.const 4096) "{{}}")
			(func (export "serein_alloc") (param i32) (result i32) (i32.const 8192))
			(func (export "serein_invoke") (param i32 i32) (result i64) {body}
				(i64.const {})))"#,
			(4096_u64 << 32) | 2,
		))
		.unwrap();
		package
	}

	/// A plugin writes to its private folder through the real worker and import, a pick
	/// needs the `files` grant, a saved pick is listed with its name and can be removed,
	/// and deleting the plugin's data removes the folder with it.
	#[test]
	fn plugins_write_their_folder_and_picks_need_the_grant() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let gate = gate();
		let header = r#"{"root":"private","path":"out.txt"}"#;
		let escaped: String = header.bytes().map(|b| format!("\\{b:02x}")).collect();
		let mut filer = topic_plugin(
			"filer",
			true,
			r#"(import "serein" "file_write" (func $write (param i32 i32 i32 i32) (result i32)))"#,
			&format!(r#"(data (i32.const 1024) "{escaped}") (data (i32.const 2048) "hello")"#),
			&format!(
				"(drop (call $write (i32.const 1024) (i32.const {}) (i32.const 2048) (i32.const 5)))",
				header.len()
			),
		);
		filer.manifest.broadcasts.clear();
		filer.manifest.capabilities = vec![Capability::Files];
		let plain = topic_plugin("plain", true, "", "", "");
		enable(
			&root,
			source(&profile, &filer),
			vec![Capability::Files],
			Some("account"),
			&gate,
		)
		.unwrap();
		enable(
			&root,
			source(&profile, &plain),
			Vec::new(),
			Some("account"),
			&gate,
		)
		.unwrap();
		let Ok(Event::Invoked { .. }) = run(
			&root,
			Job::Invoke {
				id: "filer".into(),
				account: "account".into(),
				invocation: Invocation {
					action: "run".into(),
					..Default::default()
				},
			},
			&gate,
		) else {
			panic!("not invoked")
		};
		let data = data_dir(&root, "filer", Some("account")).unwrap();
		assert_eq!(
			fs::read(data.join(crate::files::PRIVATE_FOLDER).join("out.txt")).unwrap(),
			b"hello"
		);

		let picked = profile.0.join("Exports");
		fs::create_dir_all(&picked).unwrap();
		let pick = |id: &str| Job::AddFileHandle {
			id: id.into(),
			account: Some("account".into()),
			connection: false,
			pick: extensions::FilePick {
				kind: extensions::PickKind::Folder,
				write: true,
				purpose: "Exports".into(),
				name: None,
				action: "run".into(),
			},
			path: picked.clone(),
		};
		assert!(run(&root, pick("plain"), &gate).is_err(), "no files grant");
		let Ok(Event::FileHandleAdded {
			root: handle,
			usage,
			..
		}) = run(&root, pick("filer"), &gate)
		else {
			panic!("pick not saved")
		};
		let listed = &usage.iter().find(|data| data.id == "filer").unwrap();
		assert_eq!(listed.handles[0].root, handle);
		assert_eq!(listed.handles[0].name, "Exports");
		assert_eq!(
			listed.bytes,
			5 + fs::metadata(data.join(SETTINGS_FILE)).unwrap().len()
		);
		let Ok(Event::DataUsage(usage)) = run(
			&root,
			Job::RemoveFileHandle {
				id: "filer".into(),
				account: Some("account".into()),
				connection: false,
				root: handle,
			},
			&gate,
		) else {
			panic!("not removed")
		};
		assert!(
			usage
				.iter()
				.find(|data| data.id == "filer")
				.unwrap()
				.handles
				.is_empty()
		);
		assert!(picked.exists(), "removing access leaves the folder alone");
		delete_data_dir(&data).unwrap();
		assert!(!data.join(crate::files::PRIVATE_FOLDER).exists());
	}

	/// A media worker keeps one instance across batches: the plugin's memory counts them,
	/// each batch's bytes come through `media_read`, and its captions come back. A batch
	/// the plugin cannot take ends the worker, and a plugin without the grant never starts.
	#[test]
	fn media_workers_keep_their_instance_and_read_batch_data() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let gate = gate();
		let one = r#"{"captions":[{"text":"one"}]}"#;
		let two = r#"{"captions":[{"text":"two"}]}"#;
		let wat = |text: &str| text.replace('"', "\\\"");
		let mut package = database_plugin("{}");
		package.manifest.id = "media".into();
		package.manifest.capabilities = vec![Capability::MediaWorker, Capability::CallCaptions];
		package.manifest.actions[0].surface = Surface::Media;
		package.wasm = wat::parse_str(format!(
			r#"(module
			(import "serein" "media_read" (func $read (param i32 i32 i32) (result i32)))
			(memory (export "memory") 2)
			(global $n (mut i32) (i32.const 0))
			(data (i32.const 1024) "{}")
			(data (i32.const 2048) "{}")
			(func (export "serein_alloc") (param i32) (result i32) (i32.const 8192))
			(func (export "serein_invoke") (param i32 i32) (result i64)
				(if (i32.ne (call $read (i32.const 60000) (i32.const 0) (i32.const 5)) (i32.const 5))
					(then (return (i64.const 0))))
				(if (i32.ne (i32.load8_u (i32.const 60000)) (i32.const 104))
					(then (return (i64.const 0))))
				(global.set $n (i32.add (global.get $n) (i32.const 1)))
				(if (result i64) (i32.eq (global.get $n) (i32.const 1))
					(then (i64.const {}))
					(else (i64.const {})))))"#,
			wat(one),
			wat(two),
			(1024_u64 << 32) | one.len() as u64,
			(2048_u64 << 32) | two.len() as u64,
		))
		.unwrap();
		let grants = package.manifest.capabilities.clone();
		enable(
			&root,
			source(&profile, &package),
			grants,
			Some("account"),
			&gate,
		)
		.unwrap();
		let plain = topic_plugin("plain", true, "", "", "");
		enable(
			&root,
			source(&profile, &plain),
			Vec::new(),
			Some("account"),
			&gate,
		)
		.unwrap();
		let host = ExtensionHost::new(root);
		let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
		let chunk = |length| extensions::MediaItem::Chunk {
			stream: "user:1".into(),
			codec: "opus".into(),
			time_us: 0,
			offset: 0,
			length,
			rtp_timestamp: None,
			sequence: None,
			keyframe: false,
		};
		let worker = host.start_media("media", "account", Arc::clone(&wake));
		worker.push(vec![chunk(5)], b"hello".to_vec());
		worker.push(vec![chunk(5)], b"hello".to_vec());
		let text = |event| match event {
			Some(crate::media::MediaEvent::Captions(lines)) => lines[0].text.clone(),
			other => panic!("{other:?}"),
		};
		assert_eq!(text(worker.wait_event()), "one");
		assert_eq!(
			text(worker.wait_event()),
			"two",
			"the instance kept its count"
		);
		// A chunk past the batch's data is refused before the plugin runs, ending the worker.
		worker.push(vec![chunk(5)], b"hi".to_vec());
		assert!(matches!(
			worker.wait_event(),
			Some(crate::media::MediaEvent::Failed(_))
		));
		let refused = host.start_media("plain", "account", wake);
		assert!(matches!(
			refused.wait_event(),
			Some(crate::media::MediaEvent::Failed(error)) if error.contains("not granted")
		));
	}

	/// Catalog sources are saved without their token, which goes to the secret store; an
	/// edited address forgets it; a cached source catalog is merged into what loads; and
	/// removing a source forgets its token and cache.
	#[test]
	fn catalog_sources_keep_tokens_in_the_secret_store_and_merge_their_entries() {
		secrets::install(Arc::new(secrets::MemoryStore::default()));
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let gate = gate();
		let set = |source: crate::sources::CatalogSource, token: Option<Option<&str>>| {
			run(
				&root,
				Job::SetCatalogSource {
					source,
					token: token
						.map(|token| token.map(|token| zeroize::Zeroizing::new(token.into()))),
				},
				&gate,
			)
		};
		let team = |url: &str| crate::sources::CatalogSource {
			id: String::new(),
			name: "Team".into(),
			url: url.into(),
			token_origins: Vec::new(),
			auto_refresh: false,
			has_token: false,
		};
		let Ok(Event::CatalogSources(sources)) = set(
			team("https://example.org/catalog.json"),
			Some(Some("ghp_secret")),
		) else {
			panic!("not saved")
		};
		let saved = sources[0].source.clone();
		assert_eq!((saved.id.as_str(), saved.has_token), ("team", true));
		let file = fs::read_to_string(root.join(crate::sources::SOURCES_FILE)).unwrap();
		assert!(
			!file.contains("ghp_secret"),
			"the token never reaches the sources file"
		);
		let key = crate::sources::token_key("team");
		assert_eq!(
			secrets::store()
				.unwrap()
				.load(&key)
				.unwrap()
				.as_deref()
				.map(String::as_str),
			Some("ghp_secret")
		);
		// Same address, no new token: kept. A new address: forgotten.
		let Ok(Event::CatalogSources(sources)) = set(saved.clone(), None) else {
			panic!()
		};
		assert!(sources[0].source.has_token);
		let moved = crate::sources::CatalogSource {
			url: "https://elsewhere.example.org/catalog.json".into(),
			..saved.clone()
		};
		let Ok(Event::CatalogSources(sources)) = set(moved.clone(), None) else {
			panic!()
		};
		assert!(!sources[0].source.has_token);
		assert!(secrets::store().unwrap().load(&key).unwrap().is_none());

		// A cached source catalog loads merged and tagged.
		let catalog = serde_json::json!({"api_version": 1, "entries": [{
			"manifest": {
				"api_version": 1, "id": "private-plugin", "name": "Private", "version": "1.0.0",
				"author": "team", "license": "MIT", "source": "https://example.org",
				"kind": "plugin", "capabilities": [],
				"actions": [{"id": "run", "label": "Run", "surface": "panel"}]
			},
			"release_url": "https://elsewhere.example.org/p.serein-extension",
			"sha256": "a".repeat(64),
			"download_bytes": 1,
			"source_commit": "b".repeat(40),
		}]});
		let cache = crate::sources::cache_path(&root, "team");
		fs::create_dir_all(cache.parent().unwrap()).unwrap();
		fs::write(&cache, serde_json::to_vec(&catalog).unwrap()).unwrap();
		let Ok(Event::Loaded {
			catalog, sources, ..
		}) = run(&root, Job::Load { account: None }, &gate)
		else {
			panic!("not loaded")
		};
		let entry = &catalog.unwrap().entries[0];
		assert_eq!(entry.catalog_source.as_deref(), Some("team"));
		assert_eq!(sources[0].entries, 1);

		set(moved, Some(Some("again"))).unwrap();
		let Ok(Event::CatalogSources(sources)) =
			run(&root, Job::RemoveCatalogSource { id: "team".into() }, &gate)
		else {
			panic!()
		};
		assert!(sources.is_empty() && !cache.exists());
		assert!(secrets::store().unwrap().load(&key).unwrap().is_none());
	}

	/// A `media_archive` plugin queues an attachment through the real import; the archive
	/// saves it into the private folder and the `fetches` request reports it. A link that is
	/// not a Discord attachment is refused at the call. The downloader is synthetic.
	#[test]
	fn archive_plugins_queue_cdn_attachments_into_their_folder() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let archive = Arc::new(crate::archive::Archive::new(Arc::new(
			|fetch, partial, _| {
				fs::write(partial, vec![1; fetch.size as usize]).map_err(|_| "write".to_string())
			},
		)));
		let gate = Gate {
			archive: Arc::clone(&archive),
			..gate()
		};
		let plugin = |id: &str, url: &str| {
			let request = format!(r#"{{"url":"{url}","size":3,"path":"saved/a.png"}}"#);
			let escaped: String = request.bytes().map(|b| format!("\\{b:02x}")).collect();
			let mut package = topic_plugin(
				id,
				true,
				r#"(import "serein" "attachment_fetch" (func $fetch (param i32 i32) (result i32)))"#,
				&format!(r#"(data (i32.const 1024) "{escaped}")"#),
				&format!(
					"(if (i32.ne (call $fetch (i32.const 1024) (i32.const {})) (i32.const 0)) (then unreachable))",
					request.len()
				),
			);
			package.manifest.broadcasts.clear();
			package.manifest.capabilities = vec![Capability::Files, Capability::MediaArchive];
			package
		};
		let good = plugin(
			"archiver",
			"https://cdn.discordapp.com/attachments/1/2/a.png?ex=1&is=2&hm=3",
		);
		let bad = plugin("phisher", "https://example.org/attachments/1/2/a.png");
		for package in [&good, &bad] {
			let grants = package.manifest.capabilities.clone();
			enable(
				&root,
				source(&profile, package),
				grants,
				Some("account"),
				&gate,
			)
			.unwrap();
		}
		let invoke = |id: &str| {
			run(
				&root,
				Job::Invoke {
					id: id.into(),
					account: "account".into(),
					invocation: Invocation {
						action: "run".into(),
						..Default::default()
					},
				},
				&gate,
			)
		};
		assert!(invoke("archiver").is_ok());
		assert!(
			invoke("phisher").is_err(),
			"a non-attachment link fails the call"
		);
		let data = data_dir(&root, "archiver", Some("account")).unwrap();
		let saved = loop {
			match archive
				.results(&data)
				.first()
				.map(|(_, state)| state.clone())
			{
				Some(crate::archive::FetchState::Queued) | None => std::thread::yield_now(),
				Some(state) => break state,
			}
		};
		assert_eq!(saved, crate::archive::FetchState::Done);
		assert_eq!(
			fs::read(data.join(crate::files::PRIVATE_FOLDER).join("saved/a.png")).unwrap(),
			vec![1; 3]
		);
		let mut imports = SystemImports::for_plugin(
			&read_stored(
				&scope(&root, ExtensionKind::Plugin, Some("account"))
					.unwrap()
					.join("archiver")
					.join("package.json"),
			)
			.unwrap(),
			Some(&data),
			&gate,
		)
		.0;
		let extensions::Reply::Ok(reply) =
			extensions::HostImports::file_request(&mut *imports, br#"{"op":"fetches"}"#)
		else {
			panic!("no fetches")
		};
		let reply: serde_json::Value = serde_json::from_slice(&reply).unwrap();
		assert_eq!(reply["fetches"][0]["state"], "done");
		assert_eq!(reply["fetches"][0]["path"], "saved/a.png");
	}

	#[test]
	fn broadcasts_reach_only_approved_subscribers_and_disabling_ends_the_connection() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let gate = gate();
		let request = r#"{"topic":"serein.test/1","payload":{"n":1}}"#;
		let escaped: String = request.bytes().map(|b| format!("\\{b:02x}")).collect();
		let provider = topic_plugin(
			"provider",
			true,
			r#"(import "serein" "broadcast" (func $send (param i32 i32) (result i32)))"#,
			&format!(r#"(data (i32.const 1024) "{escaped}")"#),
			&format!(
				"(drop (call $send (i32.const 1024) (i32.const {})))",
				request.len()
			),
		);
		let subscriber = topic_plugin(
			"subscriber",
			false,
			r#"(import "serein" "topic_read" (func $read (param i32) (result i32)))"#,
			"",
			"(drop (call $read (i32.const 64)))",
		);
		let mut stranger = topic_plugin("stranger", false, "", "", "");
		stranger.manifest.subscribes = vec!["serein.other/1".into()];
		for package in [&provider, &subscriber, &stranger] {
			enable(
				&root,
				source(&profile, package),
				Vec::new(),
				Some("account"),
				&gate,
			)
			.unwrap();
		}
		let subscription = |subscriber: &str| Subscription {
			subscriber: subscriber.into(),
			provider: "provider".into(),
			topic: "serein.test/1".into(),
		};
		let approve = |subscription: Subscription, approved: bool| {
			run(
				&root,
				Job::SetSubscription {
					account: "account".into(),
					subscription,
					approved,
				},
				&gate,
			)
		};
		// Only plugins that share the topic can be connected.
		assert!(approve(subscription("stranger"), true).is_err());
		let Ok(Event::Subscriptions(saved)) = approve(subscription("subscriber"), true) else {
			panic!("subscription not saved")
		};
		assert_eq!(saved, [subscription("subscriber")]);
		let invoke = |id: &str| {
			run(
				&root,
				Job::Invoke {
					id: id.into(),
					account: "account".into(),
					invocation: Invocation {
						action: "run".into(),
						..Default::default()
					},
				},
				&gate,
			)
		};
		// The front end hands the broker the live subscriptions.
		invoke("provider").unwrap();
		assert!(
			gate.broker.pending().is_empty(),
			"nothing flows before the broker knows"
		);
		gate.broker
			.set_subscriptions(saved.iter().cloned().collect());
		invoke("provider").unwrap();
		assert_eq!(gate.broker.pending(), ["subscriber"]);
		assert!(matches!(
			invoke("subscriber"),
			Ok(Event::TopicDelivered { more: false, .. })
		));
		assert!(gate.broker.pending().is_empty());
		// Disabling either side removes the saved subscription.
		run(
			&root,
			Job::Disable {
				id: "provider".into(),
				kind: ExtensionKind::Plugin,
				account: Some("account".into()),
				delete_data: true,
			},
			&gate,
		)
		.unwrap();
		assert!(read_subscriptions(&root, "account").is_empty());
	}

	#[test]
	fn data_kept_inside_a_package_directory_moves_to_the_data_directory() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let package = database_plugin("{}");
		enable(
			&root,
			source(&profile, &package),
			vec![Capability::Database],
			Some("account"),
			&gate(),
		)
		.unwrap();
		let directory = scope(&root, ExtensionKind::Plugin, Some("account"))
			.unwrap()
			.join("database-plugin");
		let data = data_dir(&root, "database-plugin", Some("account")).unwrap();
		// The layout of earlier versions: data beside package.json.
		fs::write(directory.join("data.json"), b"saved").unwrap();
		load(&root, Some("account"), &gate()).unwrap();
		assert!(!directory.join("data.json").exists());
		assert_eq!(fs::read(data.join("data.json")).unwrap(), b"saved");
		// An existing new copy is never overwritten by a stale old one.
		fs::write(directory.join("data.json"), b"stale").unwrap();
		load(&root, Some("account"), &gate()).unwrap();
		assert_eq!(fs::read(data.join("data.json")).unwrap(), b"saved");
	}

	/// A journal plugin that reads one entry per delivery and returns `output`.
	fn journal_plugin(output: &str) -> Package {
		let escaped: String = output.bytes().map(|b| format!("\\{b:02x}")).collect();
		let wasm = wat::parse_str(format!(
			r#"(module
			(import "serein" "journal_read" (func $read (param i32) (result i32)))
			(memory (export "memory") 1)
			(data (i32.const 4096) "{escaped}")
			(func (export "serein_alloc") (param i32) (result i32) (i32.const 8192))
			(func (export "serein_invoke") (param i32 i32) (result i64)
				(drop (call $read (i32.const 1)))
				(i64.const {})))"#,
			(4096_u64 << 32) | output.len() as u64,
		))
		.unwrap();
		Package {
			manifest: Manifest {
				api_version: extensions::API_VERSION,
				id: "journal-plugin".into(),
				name: "Journal plugin".into(),
				version: "1.0.0".into(),
				author: "Synthetic test".into(),
				license: "MIT".into(),
				source: "https://github.com/example/plugin".into(),
				kind: ExtensionKind::Plugin,
				network: Vec::new(),
				secrets: Vec::new(),
				broadcasts: Vec::new(),
				subscribes: Vec::new(),
				capabilities: vec![Capability::MessageJournal],
				actions: vec![extensions::Action {
					id: "journal".into(),
					label: "Journal".into(),
					surface: Surface::Journal,
				}],
			},
			theme: None,
			background_image: Vec::new(),
			cover_image: Vec::new(),
			wasm,
		}
	}

	#[test]
	fn journal_cursor_moves_only_for_accepted_deliveries() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let install = |package: &Package| {
			enable(
				&root,
				source(&profile, package),
				vec![Capability::MessageJournal],
				Some("account"),
				&gate(),
			)
			.unwrap()
		};
		install(&journal_plugin("{}"));
		let path = data_scope(&root, Some("account"))
			.unwrap()
			.join(journal::FILE_NAME);
		let recorder =
			journal::Recorder::open(path.clone(), vec!["journal-plugin".into()], Arc::new(|| {}));
		recorder.record(
			["one", "two"]
				.map(|text| journal::Record::Entry {
					body: serde_json::json!({ "kind": "create", "text": text }).to_string(),
					latest: None,
				})
				.into(),
		);
		drop(recorder);
		let deliver = || {
			run(
				&root,
				Job::Invoke {
					id: "journal-plugin".into(),
					account: "account".into(),
					invocation: Invocation {
						action: "journal".into(),
						..Default::default()
					},
				},
				&gate(),
			)
		};
		let unread = || {
			let mut session = JournalSession::new(path.clone(), "journal-plugin".into());
			match session.read(256) {
				extensions::Reply::Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
					.unwrap()["entries"]
					.as_array()
					.unwrap()
					.len(),
				_ => panic!("journal read failed"),
			}
		};
		assert_eq!(unread(), 2);
		// Output the host rejects (storage without its grant) leaves the cursor in place.
		install(&journal_plugin(r#"{"storage":"x"}"#));
		assert!(deliver().is_err());
		assert_eq!(unread(), 2);
		install(&journal_plugin("{}"));
		assert!(matches!(
			deliver(),
			Ok(Event::JournalDelivered { more: true, .. })
		));
		assert_eq!(unread(), 1);
		assert!(matches!(
			deliver(),
			Ok(Event::JournalDelivered { more: false, .. })
		));
		assert_eq!(unread(), 0);
		// Without the grant, the journal action does not run.
		let package_path = scope(&root, ExtensionKind::Plugin, Some("account"))
			.unwrap()
			.join("journal-plugin/package.json");
		let mut stored = read_stored(&package_path).unwrap();
		stored.grants.clear();
		fs::write(&package_path, serde_json::to_vec(&stored).unwrap()).unwrap();
		assert!(deliver().is_err());
	}

	#[test]
	fn local_theme_cover_survives_edit_without_another_copy() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let mut package = theme("local-cover");
		package.cover_image = include_bytes!("../../../extensions/previews/ocean.png").to_vec();
		let Event::Enabled(first) = run(
			&root,
			Job::SaveTheme {
				package: Box::new(package),
			},
			&gate(),
		)
		.unwrap() else {
			panic!("theme save did not install");
		};
		assert!(first.local_theme && first.cover_image.is_some());
		let Event::EditTheme {
			mut package,
			local_theme,
			cover,
			..
		} = run(
			&root,
			Job::EditTheme {
				id: "local-cover".into(),
				preview: false,
			},
			&gate(),
		)
		.unwrap()
		else {
			panic!("installed theme did not open");
		};
		assert!(local_theme && cover.is_some());
		package.manifest.name = "Updated cover".into();
		let Event::Enabled(updated) = run(&root, Job::SaveTheme { package }, &gate()).unwrap()
		else {
			panic!("theme edit did not save");
		};
		assert_eq!(updated.manifest.id, "local-cover");
		assert_eq!(updated.manifest.name, "Updated cover");
		assert!(updated.cover_image.is_some());
		assert_eq!(count_directories(&root.join("themes")).unwrap(), 1);

		let imported = source(&profile, &theme("imported"));
		enable(&root, imported, Vec::new(), None, &gate()).unwrap();
		assert!(
			run(
				&root,
				Job::SaveTheme {
					package: Box::new(theme("imported"))
				},
				&gate()
			)
			.is_err()
		);
	}

	#[test]
	fn catalog_cache_is_bounded_and_does_not_change_installed_themes() {
		let profile = Profile::new();
		let bytes = include_bytes!("../../../extensions/catalog.json");
		let catalog = cache_catalog(&profile.0, bytes, &gate()).unwrap();
		let entries = catalog.entries.len();
		assert!(entries > 0);
		atomic_write(
			&profile.0.join("catalog.json"),
			&serde_json::to_vec(&catalog).unwrap(),
			&gate(),
		)
		.unwrap();
		let installed = enable(
			&profile.0,
			source(&profile, &theme("local")),
			Vec::new(),
			None,
			&gate(),
		)
		.unwrap();
		let Event::Loaded {
			catalog,
			installed: reloaded,
			..
		} = run(&profile.0, Job::Load { account: None }, &gate()).unwrap()
		else {
			panic!("expected load")
		};
		assert_eq!(catalog.unwrap().entries.len(), entries);
		assert_eq!(reloaded[0].sha256, installed.sha256);
		assert!(reloaded[0].active_theme);
		cache_catalog(&profile.0, br#"{"api_version":1,"entries":[]}"#, &gate()).unwrap();
		let Event::Loaded {
			catalog, installed, ..
		} = run(&profile.0, Job::Load { account: None }, &gate()).unwrap()
		else {
			panic!("expected load")
		};
		assert!(catalog.unwrap().entries.is_empty());
		assert!(installed[0].active_theme);
		for invalid in [b"invalid".to_vec(), vec![b' '; MAX_CATALOG + 1]] {
			cache_catalog(&profile.0, bytes, &gate()).unwrap();
			assert!(cache_catalog(&profile.0, &invalid, &gate()).is_err());
			assert_eq!(fs::read(profile.0.join("catalog.json")).unwrap(), bytes);
			fs::write(profile.0.join("catalog.json"), &invalid).unwrap();
			let Event::Loaded {
				catalog, installed, ..
			} = run(&profile.0, Job::Load { account: None }, &gate()).unwrap()
			else {
				panic!("expected load")
			};
			assert!(catalog.is_none());
			assert!(installed[0].active_theme);
		}
	}

	#[test]
	fn reviewed_theme_updates_preserve_selection_and_reject_local_collisions() {
		let profile = Profile::new();
		let themes: Vec<_> = starters()
			.unwrap()
			.into_iter()
			.filter(|entry| entry.theme.is_some())
			.take(2)
			.collect();
		let first = enable(
			&profile.0,
			themes[0].source.clone(),
			Vec::new(),
			None,
			&gate(),
		)
		.unwrap();
		let second = enable(
			&profile.0,
			themes[1].source.clone(),
			Vec::new(),
			None,
			&gate(),
		)
		.unwrap();
		let updated = enable(
			&profile.0,
			themes[0].source.clone(),
			Vec::new(),
			None,
			&gate(),
		)
		.unwrap();
		assert!(!updated.active_theme);
		assert_eq!(
			load(&profile.0, None, &gate())
				.unwrap()
				.iter()
				.find(|entry| entry.active_theme)
				.unwrap()
				.manifest
				.id,
			second.manifest.id
		);
		run(&profile.0, Job::SelectTheme { id: None }, &gate()).unwrap();
		enable(
			&profile.0,
			themes[0].source.clone(),
			Vec::new(),
			None,
			&gate(),
		)
		.unwrap();
		assert!(
			load(&profile.0, None, &gate())
				.unwrap()
				.iter()
				.all(|entry| !entry.active_theme)
		);
		let local = source(&profile, &theme(&first.manifest.id));
		enable(&profile.0, local, Vec::new(), None, &gate()).unwrap();
		assert!(
			enable(
				&profile.0,
				themes[0].source.clone(),
				Vec::new(),
				None,
				&gate()
			)
			.is_err()
		);
	}

	#[test]
	fn theme_install_select_disable_restart_and_checksum() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let first = source(&profile, &theme("first"));
		let enabled = enable(&root, first.clone(), Vec::new(), None, &gate()).unwrap();
		let original = fs::read(profile.0.join("first.json")).unwrap();
		assert_eq!(enabled.sha256, digest(&original));
		assert_eq!(enabled.download_bytes, original.len() as u64);
		let reloaded = load(&root, None, &gate()).unwrap();
		assert_eq!(reloaded[0].sha256, enabled.sha256);
		assert_eq!(reloaded[0].download_bytes, enabled.download_bytes);
		assert_eq!(load(&root, None, &gate()).unwrap()[0].manifest.id, "first");
		let second = source(&profile, &theme("second"));
		enable(&root, second, Vec::new(), None, &gate()).unwrap();
		assert!(root.join("themes/first").exists());
		let installed = load(&root, None, &gate()).unwrap();
		assert_eq!(installed.len(), 2);
		assert_eq!(
			installed
				.iter()
				.find(|entry| entry.active_theme)
				.unwrap()
				.manifest
				.id,
			"second"
		);
		run(
			&root,
			Job::SelectTheme {
				id: Some("first".into()),
			},
			&gate(),
		)
		.unwrap();
		assert_eq!(
			load(&root, None, &gate())
				.unwrap()
				.iter()
				.find(|entry| entry.active_theme)
				.unwrap()
				.manifest
				.id,
			"first"
		);
		run(&root, Job::SelectTheme { id: None }, &gate()).unwrap();
		assert!(
			load(&root, None, &gate())
				.unwrap()
				.iter()
				.all(|entry| !entry.active_theme)
		);
		disable(&root.join("themes"), "second", &gate()).unwrap();
		assert_eq!(load(&root, None, &gate()).unwrap().len(), 1);
		assert!(!root.join("themes/second").exists());
		assert!(!root.join("themes/active.json").exists());
		if let InstallSource::Local { path, .. } = &first {
			fs::write(path, b"{}").unwrap();
		}
		assert!(enable(&root, first, Vec::new(), None, &gate()).is_err());
		assert!(
			profile.0.join("first.json").exists(),
			"The user's imported original survives disable"
		);
	}

	#[test]
	fn cleanup_retries_tombstones_and_accounts_remain_isolated() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let first = scope(&root, ExtensionKind::Plugin, Some("account-one")).unwrap();
		let second = scope(&root, ExtensionKind::Plugin, Some("account-two")).unwrap();
		fs::create_dir_all(first.join("plugin")).unwrap();
		fs::create_dir_all(second.join("plugin")).unwrap();
		fs::write(first.join("plugin/data.json"), b"private-one").unwrap();
		fs::write(second.join("plugin/data.json"), b"private-two").unwrap();
		fs::write(first.with_extension("disabled"), b"").unwrap();
		cleanup(&root.join("accounts"), MAX_ACCOUNTS, &gate()).unwrap();
		assert!(!first.exists());
		assert_eq!(
			fs::read(second.join("plugin/data.json")).unwrap(),
			b"private-two"
		);
		fs::write(second.join("plugin.disabled"), b"").unwrap();
		fs::write(second.join("orphan.partial"), b"unfinished").unwrap();
		cleanup(&second, MAX_PER_SCOPE, &gate()).unwrap();
		assert!(!second.join("plugin").exists());
		assert!(!second.join("orphan.partial").exists());
		assert_eq!(
			scope(&root, ExtensionKind::Plugin, None).unwrap(),
			root.join("proxy-plugins")
		);
		assert!(disable(&second, "../outside", &gate()).is_err());
	}

	#[test]
	fn invalid_package_remains_disableable_and_cancel_preserves_cleanup() {
		let profile = Profile::new();
		let root = profile.0.join("extensions");
		let directory = scope(&root, ExtensionKind::Plugin, Some("account")).unwrap();
		fs::create_dir_all(directory.join("broken")).unwrap();
		fs::write(directory.join("broken/package.json"), b"invalid").unwrap();
		let loaded = load(&root, Some("account"), &gate()).unwrap();
		assert_eq!(loaded.len(), 1);
		assert!(loaded[0].error.is_some());
		disable(&directory, "broken", &gate()).unwrap();
		let mut host = ExtensionHost::new(root);
		host.queue.push_back((0, Job::RefreshCatalog));
		host.queue.push_back((
			1,
			Job::Logout {
				account: "account".into(),
			},
		));
		host.cancel();
		assert_eq!(host.queue.len(), 1);
		assert!(matches!(host.queue.front(), Some((1, Job::Logout { .. }))));
		let cancelled = gate();
		cancelled.generation.fetch_add(1, Ordering::Release);
		assert!(atomic_write(&profile.0.join("cancelled.json"), b"no", &cancelled).is_err());
		assert!(!profile.0.join("cancelled.json").exists());
	}

	#[test]
	fn shop_preview_checks_hash_size_format_and_decode_bounds() {
		fn encoded(width: u32, height: u32, format: image::ImageFormat) -> Vec<u8> {
			let mut output = Cursor::new(Vec::new());
			image::DynamicImage::new_rgb8(width, height)
				.write_to(&mut output, format)
				.unwrap();
			output.into_inner()
		}
		fn metadata(bytes: &[u8]) -> Preview {
			Preview {
				url: "https://example.org/preview.png".into(),
				sha256: digest(bytes),
				download_bytes: bytes.len() as u64,
			}
		}
		for format in [image::ImageFormat::Png, image::ImageFormat::Jpeg] {
			let bytes = encoded(1280, 720, format);
			let mut preview = metadata(&bytes);
			assert_eq!(
				decode_preview(&bytes, &preview).unwrap().resolution(),
				[640, 360]
			);
			preview.sha256 = "0".repeat(64);
			assert!(decode_preview(&bytes, &preview).is_none());
			preview = metadata(&bytes);
			preview.download_bytes += 1;
			assert!(decode_preview(&bytes, &preview).is_none());
		}
		for (width, height) in [(4097, 1), (2049, 2048)] {
			let bytes = encoded(width, height, image::ImageFormat::Png);
			assert!(decode_preview(&bytes, &metadata(&bytes)).is_none());
		}
		let gif = encoded(1, 1, image::ImageFormat::Gif);
		assert!(decode_preview(&gif, &metadata(&gif)).is_none());
		let too_large = vec![0; extensions::MAX_PREVIEW_BYTES + 1];
		assert!(decode_preview(&too_large, &metadata(&too_large)).is_none());
		let cancelled = gate();
		cancelled.generation.fetch_add(1, Ordering::Release);
		assert!(load_preview(&metadata(b"bad"), &cancelled).is_none());
	}

	#[test]
	fn byte_limits_and_private_download_destinations_fail_closed() {
		let profile = Profile::new();
		let file = profile.0.join("large.json");
		fs::write(&file, b"12345").unwrap();
		assert!(read_bounded(&file, 4).is_err());
		for ip in [
			"127.0.0.1",
			"10.0.0.1",
			"192.168.0.1",
			"169.254.169.254",
			"100.64.0.1",
			"0.0.0.0",
			"::1",
			"fc00::1",
			"fe80::1",
			"::ffff:127.0.0.1",
			"2001:db8::1",
		] {
			assert!(!public_ip(ip.parse().unwrap()), "{ip}");
		}
		for ip in ["1.1.1.1", "2606:4700:4700::1111"] {
			assert!(public_ip(ip.parse().unwrap()), "{ip}");
		}
		let cancelled = gate();
		cancelled.generation.fetch_add(1, Ordering::Release);
		assert!(
			download(
				"https://github.com/example",
				100,
				&cancelled,
				Duration::from_secs(60),
				None,
			)
			.is_err()
		);
	}
}
