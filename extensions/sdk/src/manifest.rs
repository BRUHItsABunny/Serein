use serde::{Deserialize, Serialize};

/// Package metadata for authoring tools and tests, not an invocation or a permission grant.
/// Serde checks the document shape; the host remains responsible for semantic validation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
	pub api_version: u32,
	pub id: String,
	pub name: String,
	pub version: String,
	pub author: String,
	pub license: String,
	pub source: String,
	pub kind: ExtensionKind,
	#[serde(default)]
	pub capabilities: Vec<Capability>,
	#[serde(default)]
	pub actions: Vec<Action>,
	/// HTTPS origins [`crate::host::https`] may reach, such as `https://api.example.org`;
	/// requires the `network` capability.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub network: Vec<String>,
	/// Values the user enters, such as API keys. Reference one as `{{secret.<id>}}` in a
	/// [`crate::host::https`] request; the host fills it in for its origins only and the
	/// plugin never reads it.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub secrets: Vec<SecretSpec>,
	/// Topics [`crate::host::topics::broadcast`] may send, such as `serein.captions/1`.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub broadcasts: Vec<String>,
	/// Topics this plugin receives in its [`Surface::Topic`] action.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub subscribes: Vec<String>,
}

/// A secret the user enters for the plugin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
	/// Lowercase letters, digits and `_`, at most 32.
	pub id: String,
	/// Shown where the user enters the value.
	pub label: String,
	/// Origins from `network` that may receive it.
	pub origins: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionKind {
	Plugin,
	Theme,
}

/// Explicit permissions requested by a manifest. Each still requires user consent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
	RichPresence,
	RelationshipControl,
	AccountControl,
	AudioSettings,
	VoiceConnect,
	CameraControl,

	MessageSend,
	MessageManage,
	ReactionsControl,
	ReadStateControl,
	ThreadsControl,
	ChannelControl,
	ServerControl,
	RoleControl,
	ModerationControl,
	MediaControl,
	ActionFeedback,
	DataQueries,
	MessagingSettings,
	GuildFolders,

	MessageContent,
	ForumData,
	ConversationActivity,
	ChannelMetadata,
	MemberDetails,
	SelectedMessage,
	Composer,
	Storage,
	DeletedMessages,
	ImageSharing,
	Appearance,
	MessageEvents,
	AppContext,
	ChannelDirectory,
	Timeline,
	Members,
	Presence,
	VoiceState,
	ReadState,
	LocalSettings,
	NotificationSettings,
	Navigation,
	LocalNotices,
	ClipboardWrite,
	VoiceControl,
	AppEvents,
	AccountProfile,
	GuildDirectory,
	ChannelDetails,
	DataEvents,
	MessageDetails,
	Relationships,
	ExternalNavigation,
	ContextMenus,
	InterfaceTweaks,
	InterfaceUi,
	StartScreen,
	/// Import [`crate::host::now_ms`].
	Clock,
	/// Import [`crate::host::fill_random`].
	Random,
	/// Use [`crate::host::db`], a private SQLite database.
	Database,
	/// Use [`crate::host::journal`] in a [`Surface::Journal`] action.
	MessageJournal,
	/// Use [`crate::host::https`] for the manifest's `network` origins.
	Network,
	/// Rewrite drafts in a [`Surface::Send`] action; see [`crate::SendInvocation`].
	SendHook,
	/// Return [`crate::Annotation`]s from [`Surface::Annotate`] and [`Surface::Message`] actions.
	MessageAnnotations,
	/// Reword or hide desktop alerts in a [`Surface::Notification`] action; see
	/// [`crate::NotificationInvocation`].
	Notifications,
	/// Show a [`crate::Page`] in the main area from activation and panel actions; see
	/// [`crate::PageOutput`].
	Pages,
	/// Show caption lines above the call controls; see [`crate::CaptionOutput`].
	CallCaptions,
	/// Use [`crate::host::files`]: a private folder and folders or files the user picks.
	Files,
	/// Stay loaded during calls and receive [`crate::MediaBatch`]es in a
	/// [`Surface::Media`] action.
	MediaWorker,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
	Message,
	Composer,
	Panel,
	Activation,
	MessageEvent,
	AppEvent,
	Tick,
	User,
	Channel,
	Server,
	Role,
	/// Host-scheduled message journal delivery; read entries with [`crate::host::journal::read`].
	Journal,
	/// Host-scheduled delivery of approved broadcasts; read them with [`crate::host::topics::read`].
	Topic,
	/// The user is sending a message in a channel where they switched this hook on; decode
	/// [`crate::SendInvocation`] and return the rewritten draft in `replacement`.
	Send,
	/// Host-scheduled batches of messages to annotate; decode [`crate::AnnotateInvocation`]
	/// and return [`crate::AnnotationOutput`].
	Annotate,
	/// Runs before each desktop alert; decode [`crate::NotificationInvocation`] and return
	/// [`crate::NotificationOutput`].
	Notification,
	/// Runs once per batch in an instance kept loaded during a call; decode
	/// [`crate::MediaInvocation`]. It may return only captions.
	Media,
}

/// Declared entry point; its ID arrives in `Invocation::action`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
	pub id: String,
	pub label: String,
	pub surface: Surface,
}
