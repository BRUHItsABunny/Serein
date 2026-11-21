//! Explicitly granted, bounded snapshots of already-loaded client data. No IO.
use client_core::{State, auth::AuthState};
use extensions::*;
use model::{Freshness, Id};

pub fn available(state: &State) -> bool {
	(state.demo || state.auth == AuthState::Authenticated)
		&& state.user.as_ref().is_some_and(|user| user.id.0 != 0)
}

pub fn uses_app(capabilities: &[Capability]) -> bool {
	capabilities.iter().any(|capability| {
		matches!(
			capability,
			Capability::AppContext
				| Capability::MessageContent
				| Capability::ForumData
				| Capability::ConversationActivity
				| Capability::AccountProfile
				| Capability::GuildDirectory
				| Capability::ChannelDetails
				| Capability::ChannelMetadata
				| Capability::MemberDetails
				| Capability::DataEvents
				| Capability::MessageDetails
				| Capability::Relationships
				| Capability::ChannelDirectory
				| Capability::Timeline
				| Capability::Members
				| Capability::Presence
				| Capability::VoiceState
				| Capability::ReadState
				| Capability::LocalSettings
				| Capability::NotificationSettings
				| Capability::DataQueries
				| Capability::MessagingSettings
				| Capability::GuildFolders
				| Capability::Navigation
				| Capability::LocalNotices
				| Capability::ClipboardWrite
				| Capability::VoiceControl
				| Capability::AppEvents
				| Capability::MessageSend
				| Capability::MessageManage
				| Capability::ReactionsControl
				| Capability::ReadStateControl
				| Capability::ThreadsControl
				| Capability::ChannelControl
				| Capability::ServerControl
				| Capability::RoleControl
				| Capability::ModerationControl
				| Capability::MediaControl
				| Capability::RelationshipControl
				| Capability::AccountControl
				| Capability::AudioSettings
				| Capability::VoiceConnect
				| Capability::CameraControl
		)
	})
}

/// A display name bounded to 128 bytes without control characters; `None` when nothing
/// printable remains.
fn bounded_name(value: &str) -> Option<String> {
	let mut end = value.len().min(128);
	while !value.is_char_boundary(end) {
		end -= 1;
	}
	let name: String = value[..end].chars().filter(|c| !c.is_control()).collect();
	(!name.trim().is_empty()).then_some(name)
}

pub fn name(value: &str) -> String {
	bounded_name(value).unwrap_or_else(|| "Unnamed".into())
}

fn user(value: &model::User) -> UserSnapshot {
	UserSnapshot {
		id: value.id.0.to_string(),
		name: name(&value.name),
	}
}

/// A channel record; `parent_id` names only a parent the user can view.
fn channel(state: &State, value: &model::Channel) -> ChannelSnapshot {
	ChannelSnapshot {
		id: value.id.0.to_string(),
		guild_id: value.guild.map(|id| id.0.to_string()),
		name: name(&value.name),
		kind: value.kind,
		parent_id: value
			.parent_id
			.filter(|id| id.0 != 0 && state.can_view(*id))
			.map(|id| id.0.to_string()),
		position: value.position,
	}
}

pub fn text(value: &str, limit: usize) -> String {
	let mut end = value.len().min(limit);
	while !value.is_char_boundary(end) {
		end -= 1;
	}
	value[..end]
		.chars()
		.filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
		.collect()
}

fn asset_hash(value: &Option<String>) -> Option<String> {
	value
		.as_ref()
		.filter(|hash| hash.len() <= 128 && model::valid_avatar_hash(hash))
		.cloned()
}

// Each candidate is already scalar-bounded before serialization. Account for JSON and
// fixed list storage; these per-group budgets also leave room below the 64 KiB ABI cap.
pub fn push<T: serde::Serialize>(
	items: &mut Vec<T>,
	item: T,
	left: &mut usize,
	max: usize,
) -> bool {
	let bytes = serde_json::to_vec(&item).map_or(usize::MAX, |bytes| {
		bytes.len().saturating_add(std::mem::size_of::<T>())
	});
	if items.len() >= max || bytes > *left {
		return false;
	}
	*left -= bytes;
	items.push(item);
	true
}

pub fn message_detail(message: &model::Message) -> MessageDetailSnapshot {
	let mut detail = MessageDetailSnapshot {
		id: message.id.0.to_string(),
		kind: message.kind,
		reply_to: message
			.reply_to
			.filter(|id| id.0 != 0)
			.map(|id| id.0.to_string()),
		mention_ids: Vec::new(),
		mentions_truncated: false,
		mention_everyone: message.mention_everyone,
		attachments: Vec::new(),
		attachments_truncated: false,
		reactions: message.reactions.as_ref().map(|_| Vec::new()),
		reactions_truncated: false,
	};
	// Bound nested rows before allocating/serializing the complete candidate.
	let mut budget = 4 * 1024;
	for mentioned in message.mentions.iter().filter(|u| u.id.0 != 0) {
		let id = mentioned.id.0.to_string();
		if detail.mention_ids.contains(&id) {
			continue;
		}
		if !push(
			&mut detail.mention_ids,
			id,
			&mut budget,
			MAX_MESSAGE_MENTIONS,
		) {
			detail.mentions_truncated = true;
			break;
		}
	}
	for attachment in &message.attachments {
		let id = attachment.id.0.to_string();
		if attachment.id.0 == 0 || detail.attachments.iter().any(|a| a.id == id) {
			detail.attachments_truncated = true;
			continue;
		}
		let filename: String = text(&attachment.filename, 256)
			.chars()
			.filter(|c| !c.is_control())
			.collect();
		let content_type = attachment
			.content_type
			.as_deref()
			.map(|s| {
				text(s, 128)
					.chars()
					.filter(|c| !c.is_control())
					.collect::<String>()
			})
			.filter(|s| !s.is_empty());
		if !push(
			&mut detail.attachments,
			AttachmentSnapshot {
				id,
				filename: if filename.is_empty() {
					"Attachment".into()
				} else {
					filename
				},
				size: attachment.size,
				content_type,
				spoiler: attachment.spoiler,
			},
			&mut budget,
			MAX_MESSAGE_ATTACHMENTS,
		) {
			detail.attachments_truncated = true;
			break;
		}
	}
	if let (Some(source), Some(reactions)) = (&message.reactions, &mut detail.reactions) {
		for reaction in source {
			if reaction.count == 0
				|| !reaction.emoji.valid()
				|| reaction.emoji.id.is_some_and(|id| id.0 == 0)
			{
				detail.reactions_truncated = true;
				continue;
			}
			if !push(
				reactions,
				ReactionSnapshot {
					emoji_id: reaction.emoji.id.map(|id| id.0.to_string()),
					emoji_name: reaction.emoji.name.clone(),
					count: reaction.count,
					me: reaction.me,
					me_burst: reaction.me_burst,
				},
				&mut budget,
				MAX_MESSAGE_REACTIONS,
			) {
				detail.reactions_truncated = true;
				break;
			}
		}
	}
	detail
}

pub fn channel_metadata(state: &State) -> Option<ChannelMetadataSnapshot> {
	use model::permissions as p;
	if !state.gateway_connected || state.freshness != Freshness::Fresh {
		return None;
	}
	let id = state.selected?;
	let current = state.channel(id)?;
	let guild = current.guild?;
	if !state.can_view(id) || !state.can_read_history(id) {
		return None;
	}
	let related = |id| {
		state
			.channel(id)
			.filter(|c| c.guild == Some(guild) && state.can_view(c.id))
	};
	let parent = current.parent_id.and_then(related);
	let category = parent.and_then(|parent| {
		if parent.kind == 4 {
			Some(parent)
		} else {
			parent.parent_id.and_then(related).filter(|c| c.kind == 4)
		}
	});
	let loaded = state.channel_details(id);
	let post = state.post_details(id);
	let mut permissions = std::collections::BTreeMap::new();
	for (permission, bits) in [
		(ChannelPermission::ViewChannel, p::VIEW_CHANNEL),
		(
			ChannelPermission::ReadMessageHistory,
			p::READ_MESSAGE_HISTORY,
		),
		(ChannelPermission::SendMessages, p::SEND_MESSAGES),
		(
			ChannelPermission::SendMessagesInThreads,
			p::SEND_MESSAGES_IN_THREADS,
		),
		(ChannelPermission::AttachFiles, p::ATTACH_FILES),
		(ChannelPermission::EmbedLinks, p::EMBED_LINKS),
		(ChannelPermission::AddReactions, p::ADD_REACTIONS),
		(ChannelPermission::MentionEveryone, p::MENTION_EVERYONE),
		(ChannelPermission::UseExternalEmojis, p::USE_EXTERNAL_EMOJIS),
		(
			ChannelPermission::UseExternalStickers,
			p::USE_EXTERNAL_STICKERS,
		),
		(
			ChannelPermission::UseApplicationCommands,
			p::USE_APPLICATION_COMMANDS,
		),
		(ChannelPermission::ManageChannels, p::MANAGE_CHANNELS),
		(ChannelPermission::ManageMessages, p::MANAGE_MESSAGES),
		(ChannelPermission::ManageRoles, p::MANAGE_ROLES),
		(ChannelPermission::ManageThreads, p::MANAGE_THREADS),
		(
			ChannelPermission::CreatePublicThreads,
			p::CREATE_PUBLIC_THREADS,
		),
		(
			ChannelPermission::CreatePrivateThreads,
			p::CREATE_PRIVATE_THREADS,
		),
		(ChannelPermission::ManageWebhooks, p::MANAGE_WEBHOOKS),
		(ChannelPermission::Connect, p::CONNECT),
		(ChannelPermission::Speak, p::SPEAK),
		(ChannelPermission::Stream, p::STREAM),
		(ChannelPermission::MuteMembers, p::MUTE_MEMBERS),
		(ChannelPermission::DeafenMembers, p::DEAFEN_MEMBERS),
		(ChannelPermission::MoveMembers, p::MOVE_MEMBERS),
		(ChannelPermission::UseVad, p::USE_VAD),
		(ChannelPermission::PinMessages, p::PIN_MESSAGES),
	] {
		permissions.insert(permission, state.permission(id, p::VIEW_CHANNEL | bits));
	}
	Some(ChannelMetadataSnapshot {
		channel_id: id.0.to_string(),
		guild_id: guild.0.to_string(),
		parent: parent.map(|value| channel(state, value)),
		category: category.map(|value| channel(state, value)),
		topic: loaded.map(|d| text(&d.topic, 2048)),
		topic_truncated: loaded.is_some_and(|d| d.topic.len() > 2048),
		slowmode_seconds: loaded.map(|d| d.slowmode),
		nsfw: loaded.map(|d| d.nsfw),
		thread: matches!(current.kind, 10..=12).then(|| ThreadMetadataSnapshot {
			owner_id: post
				.and_then(|p| p.owner)
				.filter(|id| id.0 != 0)
				.map(|id| id.0.to_string()),
			message_count: current.message_count,
			archived: post.map(|p| p.archived),
			locked: post.map(|p| p.locked),
			pinned: post.map(|p| p.pinned),
		}),
		permissions,
	})
}

pub const PAIRED_MESSAGE_ROWS: usize = 12;
/// Channel directory bytes reserved for the focus server, beyond the shared 10 KiB.
const FOCUS_CHANNEL_BYTES: usize = 48 * 1024;

/// The server whose channels the channel directory lists in full: the dock item an
/// interface click names, else the selected dock item of the plugin's last restated
/// interface, else the natively selected server.
pub fn focus_guild(
	state: &State,
	invocation: &Invocation,
	interface: Option<&InterfaceUi>,
) -> Option<Id> {
	let dock = interface.map_or(&[][..], |tree| &tree.dock);
	let guild = |item: &InterfaceDockItem| {
		item.guild_id
			.as_deref()
			.and_then(|id| id.parse::<u64>().ok())
			.filter(|id| *id != 0)
			.map(Id)
	};
	let clicked = (invocation.action == INTERFACE_CLICK_ACTION)
		.then(|| invocation.values.get("id"))
		.flatten();
	clicked
		.and_then(|id| dock.iter().find(|item| item.id == *id))
		.and_then(guild)
		.or_else(|| dock.iter().filter(|item| item.selected).find_map(guild))
		.or_else(|| {
			state
				.selected
				.and_then(|id| state.channel(id))
				.and_then(|c| c.guild)
		})
}

/// `focus` is the server whose channels the directory lists first and in full
/// ([`focus_guild`]); `None` lists every channel on the shared bounds.
pub fn snapshot(
	state: &State,
	messaging: &dyn HostLocal,
	manifest: &Manifest,
	focus: Option<Id>,
) -> Option<Box<AppSnapshot>> {
	if !available(state) || !uses_app(&manifest.capabilities) {
		return None;
	}
	let granted = |capability| manifest.capabilities.contains(&capability);
	let connected = state.gateway_connected;
	let selected = state
		.selected
		.filter(|id| connected && state.freshness != Freshness::Unavailable && state.can_view(*id));
	let readable = selected.filter(|id| {
		state.can_read_history(*id)
			&& state.freshness == Freshness::Fresh
			&& state.channel(*id).is_some_and(|c| c.supports_text())
	});
	let mut app = AppSnapshot::default();
	if granted(Capability::AppContext) {
		app.context = Some(AppContextSnapshot {
			connected,
			user: state.user.as_ref().map(user),
			channel: selected
				.and_then(|id| state.channel(id))
				.map(|value| channel(state, value)),
		});
	}
	if connected && granted(Capability::AccountProfile) {
		let current = state.user.as_ref()?;
		let profile = state
			.own_profile
			.data
			.as_ref()
			.filter(|profile| {
				profile.user.id == current.id
					&& !profile.limited
					&& !state.own_profile.loading
					&& !state.own_profile.reload_required
					&& state.own_profile.error.is_none()
			})
			.map(|profile| OwnProfileSnapshot {
				display_name: profile.global_name.as_deref().map(name),
				bio: text(&profile.bio, 2048),
				pronouns: text(&profile.pronouns, 256)
					.chars()
					.filter(|c| !c.is_control())
					.collect(),
			});
		app.account_profile = Some(AccountProfileSnapshot {
			user: user(current),
			avatar: asset_hash(&current.avatar),
			profile,
		});
	}
	if connected && granted(Capability::GuildDirectory) {
		let mut directory = GuildDirectorySnapshot {
			items: Vec::new(),
			truncated: false,
		};
		let mut budget = 8 * 1024;
		for guild in state.guilds.iter().filter(|guild| guild.id.0 != 0) {
			if !push(
				&mut directory.items,
				GuildSnapshot {
					id: guild.id.0.to_string(),
					name: name(&guild.name),
					icon: asset_hash(&guild.icon),
				},
				&mut budget,
				MAX_APP_GUILDS,
			) {
				directory.truncated = true;
				break;
			}
		}
		app.guilds = Some(directory);
	}
	if granted(Capability::ChannelDetails)
		&& state.freshness == Freshness::Fresh
		&& let Some(value) = selected.and_then(|id| state.channel(id))
	{
		let mut recipients = Vec::new();
		let mut recipients_truncated = false;
		let mut budget = 6 * 1024;
		for recipient in value.recipients.iter().filter(|user| user.id.0 != 0) {
			if recipients
				.iter()
				.any(|item: &UserSnapshot| item.id == recipient.id.0.to_string())
			{
				continue;
			}
			if !push(
				&mut recipients,
				user(recipient),
				&mut budget,
				MAX_CHANNEL_RECIPIENTS,
			) {
				recipients_truncated = true;
				break;
			}
		}
		let can_read_history = state.can_read_history(value.id);
		app.channel_details = Some(ChannelDetailsSnapshot {
			channel: channel(state, value),
			parent_id: value
				.parent_id
				.filter(|id| state.can_view(*id))
				.map(|id| id.0.to_string()),
			position: value.position,
			last_message_id: value
				.last_message
				.filter(|id| id.0 != 0 && can_read_history)
				.map(|id| id.0.to_string()),
			message_count: can_read_history.then_some(value.message_count).flatten(),
			recipients,
			recipients_truncated,
			can_send: state.can_send(value.id),
			can_read_history,
		});
	}
	if connected && granted(Capability::ChannelDirectory) {
		let mut directory = ChannelDirectorySnapshot {
			items: Vec::new(),
			truncated: false,
		};
		let listed = |c: &&model::Channel| {
			state.can_view(c.id)
				&& !(state.selected == Some(c.id) && state.freshness == Freshness::Unavailable)
		};
		// The focus server comes first on its own budget, so it stays complete however many
		// other servers are loaded. Items otherwise keep the client's loaded order.
		if let Some(focus) = focus {
			let mut budget = FOCUS_CHANNEL_BYTES;
			for value in state
				.channels
				.iter()
				.filter(|c| c.guild == Some(focus))
				.filter(listed)
			{
				if !push(
					&mut directory.items,
					channel(state, value),
					&mut budget,
					MAX_APP_FOCUS_CHANNELS,
				) {
					directory.truncated = true;
					break;
				}
			}
		}
		let max = directory.items.len() + MAX_APP_CHANNELS;
		let mut budget = 10 * 1024;
		for value in state
			.channels
			.iter()
			.filter(|c| focus.is_none() || c.guild != focus)
			.filter(listed)
		{
			if !push(
				&mut directory.items,
				channel(state, value),
				&mut budget,
				max,
			) {
				directory.truncated = true;
				break;
			}
		}
		app.channels = Some(directory);
	}
	if let Some(id) = readable
		&& granted(Capability::Timeline)
	{
		let mut timeline = TimelineSnapshot {
			channel_id: id.0.to_string(),
			messages: Vec::new(),
			truncated: state.history_before.is_some()
				|| state.history_after.is_some()
				|| !state.older_exhausted,
		};
		// Paired text/metadata rows cost more to decode. Leave App Toolbox fuel headroom
		// for host discovery and other granted groups without changing the sandbox budget.
		let max_messages = if granted(Capability::MessageDetails) {
			PAIRED_MESSAGE_ROWS
		} else {
			MAX_APP_MESSAGES
		};
		let mut budget = 20 * 1024;
		for message in
			state.timeline.iter().rev().filter(|m| {
				m.channel == id && !m.ephemeral && m.flags & 64 == 0 && m.author.id.0 != 0
			}) {
			if message.content.len() > 4096 {
				timeline.truncated = true;
				continue;
			}
			if !push(
				&mut timeline.messages,
				MessageSnapshot {
					id: message.id.0.to_string(),
					author: user(&message.author),
					content: message.content.clone(),
					attachment_count: message.attachments.len().min(u16::MAX as usize) as u16,
					edited: message.edited,
				},
				&mut budget,
				max_messages,
			) {
				timeline.truncated = true;
				break;
			}
		}
		timeline.messages.reverse();
		app.timeline = Some(timeline);
	}
	if let Some(id) = selected {
		let members = state
			.members
			.as_ref()
			.filter(|members| members.channel == id && members.freshness == Freshness::Fresh);
		let recipients = state.channel(id).filter(|c| c.guild.is_none());
		if granted(Capability::Members) && (members.is_some() || recipients.is_some()) {
			let mut group = MembersSnapshot {
				channel_id: id.0.to_string(),
				items: Vec::new(),
				truncated: false,
			};
			let mut budget = 6 * 1024;
			let users = members
				.into_iter()
				.flat_map(|m| {
					m.slots.iter().flatten().filter_map(|slot| match slot {
						model::MemberSlot::Person(member) => Some(&member.user),
						_ => None,
					})
				})
				.chain(
					recipients
						.filter(|_| members.is_none())
						.into_iter()
						.flat_map(|c| &c.recipients),
				);
			for value in users.filter(|u| u.id.0 != 0) {
				let value = user(value);
				if group.items.iter().any(|item| item.id == value.id) {
					continue;
				}
				if !push(&mut group.items, value, &mut budget, MAX_APP_MEMBERS) {
					group.truncated = true;
					break;
				}
			}
			group.truncated |= members.is_some_and(|m| m.total > group.items.len() as u64);
			app.members = Some(group);
		}
		if granted(Capability::Presence) && (members.is_some() || recipients.is_some()) {
			let mut presence = PresenceSnapshot {
				items: Vec::new(),
				truncated: false,
			};
			let mut budget = 6 * 1024;
			let statuses = members
				.into_iter()
				.flat_map(|m| {
					m.slots
						.iter()
						.flatten()
						.filter_map(|slot| match slot {
							model::MemberSlot::Person(member) => Some(member),
							_ => None,
						})
						.filter_map(|m| m.status.as_deref().map(|status| (m.user.id, status)))
				})
				.chain(
					recipients
						.filter(|_| members.is_none())
						.into_iter()
						.flat_map(|c| &c.recipients)
						.filter_map(|u| {
							state
								.presence_for(u.id)
								.and_then(|p| p.status.as_deref())
								.map(|s| (u.id, s))
						}),
				);
			for (user, status) in statuses {
				let user_id = user.0.to_string();
				if presence.items.iter().any(|item| item.user_id == user_id) {
					continue;
				}
				if !push(
					&mut presence.items,
					PresenceEntry {
						user_id,
						status: status.into(),
					},
					&mut budget,
					MAX_APP_PRESENCES,
				) {
					presence.truncated = true;
					break;
				}
			}
			presence.truncated |= members.is_some_and(|m| m.total > presence.items.len() as u64);
			app.presence = Some(presence);
		}
	}
	if granted(Capability::VoiceState) {
		let call = state.voice.active.as_ref().filter(|call| {
			state.can_view(call.channel)
				&& !(state.selected == Some(call.channel)
					&& state.freshness == Freshness::Unavailable)
		});
		let mut roster = Vec::new();
		// The roster keeps its own byte budget, like the other groups, so names cannot
		// squeeze the call summary out of the snapshot.
		let mut budget = 16 * 1024;
		voice_roster(state, messaging, &mut |row| {
			let entry = VoiceRosterEntry {
				guild_id: row.entry.guild.0.to_string(),
				channel_id: row.entry.channel.0.to_string(),
				user_id: row.entry.participant.user.0.to_string(),
				// One gateway nickname with control characters or nothing printable must
				// not invalidate the whole snapshot; such a name is left out instead.
				name: row.name.and_then(bounded_name),
				muted: row.shown.muted,
				deafened: row.shown.deafened,
				server_muted: row.shown.server_muted,
				server_deafened: row.shown.server_deafened,
				speaking: row.speaking,
			};
			push(&mut roster, entry, &mut budget, MAX_VOICE_ROSTER)
		});
		app.voice = Some(VoiceSnapshot {
			channel_id: call.map(|c| c.channel.0.to_string()),
			phase: call.map_or("idle", |c| phase(c.phase)).into(),
			muted: call.is_some_and(|c| c.muted),
			deafened: call.is_some_and(|c| c.deafened),
			camera: call.is_some_and(|c| c.camera),
			streaming: call.is_some() && messaging.screen_sharing(),
			participants: call
				.into_iter()
				.flat_map(|c| &c.participants)
				.filter(|p| p.user.0 != 0)
				.take(MAX_VOICE_PARTICIPANTS)
				.map(|p| p.user.0.to_string())
				.collect(),
			roster,
		});
	}
	if granted(Capability::ReadState) {
		app.read_state = Some(ReadSnapshot {
			channel_id: selected.map(|id| id.0.to_string()),
			unread: selected.and_then(|id| state.unread(id)),
			mentions: selected.map_or(0, |id| state.mention_count(id)),
		});
	}
	if granted(Capability::LocalSettings) {
		app.settings = Some(messaging.local_settings());
	}
	if granted(Capability::NotificationSettings) {
		app.notification_settings = Some(messaging.notification_settings());
	}
	if granted(Capability::AudioSettings) {
		let settings = messaging.audio_settings();
		if settings.validate().is_ok() {
			app.audio_settings = Some(settings);
		}
	}
	if granted(Capability::AccountControl) {
		let presence = messaging.own_presence();
		if presence.validate().is_ok() {
			app.own_presence = Some(presence);
		}
	}
	// With the focus server's extra directory budget the fixed-budget groups can exceed the
	// snapshot cap; drop directory items from the end (other servers first) until it fits.
	// The snapshot is measured once and each popped item's serialized bytes subtracted from
	// that measure, so trimming does not re-serialize everything per pop; a final exact
	// re-check trims any drift the estimate can hide.
	let mut measured = app
		.bytes()
		.unwrap_or_else(|_| serde_json::to_vec(&app).map_or(usize::MAX, |bytes| bytes.len()));
	while measured > MAX_APP_SNAPSHOT_BYTES
		&& let Some(group) = app.channels.as_mut()
		&& let Some(item) = group.items.pop()
	{
		group.truncated = true;
		measured =
			measured.saturating_sub(serde_json::to_vec(&item).map_or(0, |bytes| bytes.len() + 1));
	}
	while matches!(app.bytes(), Err(Error::Limit))
		&& let Some(group) = app.channels.as_mut()
		&& group.items.pop().is_some()
	{
		group.truncated = true;
	}
	if granted(Capability::MessageDetails)
		&& let Some(id) = readable
	{
		let mut group = MessageDetailsSnapshot {
			channel_id: id.0.to_string(),
			items: Vec::new(),
			truncated: state.history_before.is_some()
				|| state.history_after.is_some()
				|| !state.older_exhausted,
		};
		// Leave room for group keys/scalars; item budgets include serialized bytes and storage.
		let mut budget = (MAX_APP_SNAPSHOT_BYTES
			.saturating_sub(app.bytes().ok()?)
			.saturating_sub(512))
		.min(8 * 1024);
		for message in state.timeline.iter().rev().filter(|m| {
			m.channel == id
				&& m.id.0 != 0
				&& !m.ephemeral
				&& m.flags & 64 == 0
				&& m.author.id.0 != 0
		}) {
			if !push(
				&mut group.items,
				message_detail(message),
				&mut budget,
				if granted(Capability::Timeline) {
					PAIRED_MESSAGE_ROWS
				} else {
					MAX_MESSAGE_DETAILS
				},
			) {
				group.truncated = true;
				break;
			}
		}
		group.items.reverse();
		app.message_details = Some(group);
	}
	if connected
		&& granted(Capability::Relationships)
		&& (state.friends_known()
			|| state.friend_requests_known()
			|| state.restricted_users_known())
	{
		let mut group = RelationshipsSnapshot {
			items: Vec::new(),
			truncated: false,
			friends_known: state.friends_known(),
			requests_known: state.friend_requests_known(),
			restricted_known: state.restricted_users_known(),
		};
		let mut budget = (MAX_APP_SNAPSHOT_BYTES
			.saturating_sub(app.bytes().ok()?)
			.saturating_sub(512))
		.min(4 * 1024);
		let entries = state
			.friends()
			.filter(|_| group.friends_known)
			.map(|u| (u, RelationshipKind::Friend))
			.chain(
				state
					.pending_friends()
					.filter(|_| group.requests_known)
					.map(|(u, _, incoming)| {
						(
							u,
							if *incoming {
								RelationshipKind::IncomingRequest
							} else {
								RelationshipKind::OutgoingRequest
							},
						)
					}),
			)
			.chain(
				state
					.restricted_users()
					.filter(|_| group.restricted_known)
					.map(|(u, _, ignored)| {
						(
							u,
							if *ignored {
								RelationshipKind::Ignored
							} else {
								RelationshipKind::Blocked
							},
						)
					}),
			);
		for (value, kind) in entries.filter(|(u, _)| u.id.0 != 0) {
			let value = user(value);
			if group
				.items
				.iter()
				.any(|row: &RelationshipSnapshot| row.user.id == value.id)
			{
				continue;
			}
			if !push(
				&mut group.items,
				RelationshipSnapshot { user: value, kind },
				&mut budget,
				MAX_RELATIONSHIPS,
			) {
				group.truncated = true;
				break;
			}
		}
		app.relationships = Some(group);
	}
	if granted(Capability::ChannelMetadata) {
		app.channel_metadata = channel_metadata(state);
		if matches!(app.bytes(), Err(Error::Limit)) {
			app.channel_metadata = None;
		}
	}
	if granted(Capability::MemberDetails) {
		app.member_details = crate::member_details::snapshot(state);
		while matches!(app.bytes(), Err(Error::Limit)) {
			let group = app.member_details.as_mut()?;
			if group.items.pop().is_some() {
				group.truncated = true;
			} else if group
				.roles
				.as_mut()
				.is_some_and(|roles| roles.pop().is_some())
			{
				group.roles_truncated = true;
			} else {
				app.member_details = None;
				break;
			}
		}
	}
	if granted(Capability::MessageContent) {
		app.message_content = crate::message_content::snapshot(state);
		while matches!(app.bytes(), Err(Error::Limit)) {
			let group = app.message_content.as_mut()?;
			if group.items.pop().is_some() {
				group.truncated = true;
			} else {
				app.message_content = None;
				break;
			}
		}
	}
	if granted(Capability::ForumData) {
		app.forum_data = crate::forum_data::snapshot(state);
		while matches!(app.bytes(), Err(Error::Limit)) {
			let group = app.forum_data.as_mut()?;
			if group.posts.pop().is_some() {
				group.truncated = true;
			} else {
				app.forum_data = None;
				break;
			}
		}
	}
	if granted(Capability::ConversationActivity) {
		app.conversation_activity = conversation_activity(state);
		if matches!(app.bytes(), Err(Error::Limit)) {
			app.conversation_activity = None;
		}
	}
	// Keep the boundary authoritative if model data or serialization changes later.
	app.validate(manifest).ok()?;
	Some(Box::new(app))
}

pub fn query_snapshot(state: &State, manifest: &Manifest) -> Option<Box<QuerySnapshot>> {
	if !available(state) || !manifest.capabilities.contains(&Capability::DataQueries) {
		return None;
	}
	let messages = state.search.as_ref().map(|view| {
		let page = view.page.as_ref();
		MessageQuerySnapshot {
			channel_id: view.channel.0.to_string(),
			pins: view.pins,
			query: view.query.clone(),
			loading: view.loading,
			error: view.error.map(str::to_owned),
			total: page.map_or(0, |page| page.total),
			partial: page.is_some_and(|page| page.partial),
			next: page.and_then(|page| {
				if view.pins {
					page.pin_cursor.map(|cursor| cursor.to_string())
				} else {
					page.partial
						.then(|| page.hits.last().map(|hit| hit.id.0.to_string()))
						.flatten()
				}
			}),
			items: page
				.into_iter()
				.flat_map(|page| page.hits.iter())
				.take(20)
				.map(|hit| MessageQueryItem {
					id: hit.id.0.to_string(),
					author: user(&hit.author),
					excerpt: text(&hit.excerpt, 512),
				})
				.collect(),
		}
	});
	let archives = state.archives.as_ref().map(|view| ArchiveQuerySnapshot {
		parent_id: view.parent.0.to_string(),
		kind: match view.kind {
			model::archives::Kind::Public => ArchiveQueryKind::Public,
			model::archives::Kind::Private => ArchiveQueryKind::Private,
			model::archives::Kind::JoinedPrivate => ArchiveQueryKind::JoinedPrivate,
		},
		loading: view.loading,
		error: view.error.map(str::to_owned),
		items: view
			.page
			.as_ref()
			.into_iter()
			.flat_map(|page| page.threads.iter())
			.take(20)
			.map(|value| channel(state, value))
			.collect(),
		next: view.page.as_ref().and_then(|page| {
			page.next.map(|cursor| match cursor {
				model::archives::Cursor::Time(value) => value.to_string(),
				model::archives::Cursor::Id(value) => value.0.to_string(),
			})
		}),
	});
	let member_view = &state.member_search[0];
	let members = member_view
		.request
		.as_ref()
		.map(|request| MemberQuerySnapshot {
			channel_id: request.channel.0.to_string(),
			query: request.query.clone(),
			loading: !member_view.finished && member_view.error.is_none(),
			error: member_view.error.map(str::to_owned),
			truncated: member_view.rows.len() > 20,
			items: member_view
				.rows
				.iter()
				.take(20)
				.map(|member| MemberQueryItem {
					user: user(&member.user),
					nickname: member.nick.as_ref().map(|value| text(value, 256)),
					role_ids: member
						.roles
						.iter()
						.take(32)
						.map(|id| id.0.to_string())
						.collect(),
				})
				.collect(),
		});
	let profile = state.profile.as_ref().map(|view| ProfileQuerySnapshot {
		user_id: view.user.0.to_string(),
		guild_id: view.guild.map(|id| id.0.to_string()),
		loading: view.loading,
		error: view.error.map(str::to_owned),
		data: view.data.as_ref().map(|profile| ProfileQueryData {
			user: user(&profile.user),
			display_name: profile.global_name.as_ref().map(|value| text(value, 256)),
			bio: text(&profile.bio, 4096),
			pronouns: text(&profile.pronouns, 256),
			nickname: profile
				.guild
				.as_ref()
				.and_then(|guild| guild.nick.as_ref())
				.map(|value| text(value, 256)),
			role_ids: profile
				.guild
				.as_ref()
				.into_iter()
				.flat_map(|guild| guild.roles.iter())
				.take(32)
				.map(|id| id.0.to_string())
				.collect(),
			limited: profile.limited,
		}),
	});
	let gifs = state.gifs.view.as_ref().map(|view| {
		let page = view.page.as_ref();
		GifQuerySnapshot {
			query: view.query.clone(),
			loading: view.loading,
			error: view.error.map(str::to_owned),
			truncated: page.is_some_and(|page| page.gifs.len() > 10 || page.categories.len() > 20),
			items: page
				.into_iter()
				.flat_map(|page| page.gifs.iter())
				.take(10)
				.map(|gif| GifQueryItem {
					id: gif.id.clone(),
					title: gif.title.clone(),
					url: gif.url.clone(),
					preview: gif.preview.clone(),
					width: gif.width,
					height: gif.height,
				})
				.collect(),
			categories: page
				.into_iter()
				.flat_map(|page| page.categories.iter())
				.take(20)
				.map(|category| category.name.clone())
				.collect(),
		}
	});
	let snapshot = QuerySnapshot {
		messages,
		archives,
		members,
		profile,
		gifs,
	};
	snapshot.validate().ok()?;
	Some(Box::new(snapshot))
}

pub fn messaging_settings_snapshot(
	state: &State,
	manifest: &Manifest,
) -> Option<Box<MessagingSettingsSnapshot>> {
	if !available(state)
		|| !manifest
			.capabilities
			.contains(&Capability::MessagingSettings)
	{
		return None;
	}
	let value = state.messaging_permissions.snapshot.as_ref()?;
	let truncated = value.restricted_guilds.len() > MAX_MESSAGING_SETTINGS_IDS
		|| value.unfiltered_guilds.len() > MAX_MESSAGING_SETTINGS_IDS;
	let snapshot = MessagingSettingsSnapshot {
		spam_filter: value.spam_filter.try_into().ok()?,
		default_allow_dms: value.default_allow_dms,
		restricted_guild_ids: value
			.restricted_guilds
			.iter()
			.take(MAX_MESSAGING_SETTINGS_IDS)
			.map(|id| id.0.to_string())
			.collect(),
		default_filter_requests: value.default_filter_requests,
		unfiltered_guild_ids: value
			.unfiltered_guilds
			.iter()
			.take(MAX_MESSAGING_SETTINGS_IDS)
			.map(|id| id.0.to_string())
			.collect(),
		friend_source_flags: value.friend_source_flags,
		personalized_requests: value.personalized_requests,
		game_friend_dms: value.game_friend_dms,
		game_dms: value.game_dms.try_into().ok()?,
		truncated,
	};
	snapshot.validate().ok()?;
	Some(Box::new(snapshot))
}

pub fn extended_change_key(state: &State) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut hash = std::collections::hash_map::DefaultHasher::new();
	if let Some(view) = &state.search {
		(
			view.request,
			view.loading,
			view.error,
			view.pins,
			view.channel,
		)
			.hash(&mut hash);
		if let Some(page) = &view.page {
			(page.total, page.partial, page.pin_cursor).hash(&mut hash);
			for hit in &page.hits {
				hit.id.hash(&mut hash);
			}
		}
	}
	if let Some(view) = &state.archives {
		(view.request, view.loading, view.error, view.parent).hash(&mut hash);
		if let Some(page) = &view.page {
			for channel in &page.threads {
				channel.id.hash(&mut hash);
			}
		}
	}
	let members = &state.member_search[0];
	(members.finished, members.error).hash(&mut hash);
	if let Some(request) = &members.request {
		(request.nonce, request.channel, request.query.as_str()).hash(&mut hash);
	}
	for member in &members.rows {
		member.user.id.hash(&mut hash);
	}
	if let Some(view) = &state.profile {
		(
			view.request,
			view.loading,
			view.error,
			view.user,
			view.guild,
		)
			.hash(&mut hash);
	}
	if let Some(view) = &state.gifs.view {
		(view.request, view.loading, view.error, &view.query).hash(&mut hash);
		if let Some(page) = &view.page {
			for gif in &page.gifs {
				gif.id.hash(&mut hash);
			}
			for category in &page.categories {
				category.name.hash(&mut hash);
			}
		}
	}
	if let Some(value) = &state.messaging_permissions.snapshot {
		(
			value.spam_filter,
			value.default_allow_dms,
			value.default_filter_requests,
			value.friend_source_flags,
			value.personalized_requests,
			value.game_friend_dms,
			value.game_dms,
		)
			.hash(&mut hash);
		value.restricted_guilds.hash(&mut hash);
		value.unfiltered_guilds.hash(&mut hash);
	}
	if let Some(value) = &state.guild_folders {
		value.version.hash(&mut hash);
		for folder in &value.folders {
			(folder.id, &folder.guild_ids, &folder.name, folder.color).hash(&mut hash);
		}
	}
	hash.finish()
}

pub fn guild_folders_snapshot(
	state: &State,
	manifest: &Manifest,
) -> Option<Box<GuildFoldersSnapshot>> {
	if !available(state) || !manifest.capabilities.contains(&Capability::GuildFolders) {
		return None;
	}
	let value = state.guild_folders.as_ref()?;
	let snapshot = GuildFoldersSnapshot {
		folders: value
			.folders
			.iter()
			.map(|folder| GuildFolderInput {
				id: folder.id,
				guild_ids: folder.guild_ids.iter().map(|id| id.0.to_string()).collect(),
				name: folder.name.clone(),
				color: folder.color,
			})
			.collect(),
		version: value.version,
	};
	snapshot.validate().ok()?;
	Some(Box::new(snapshot))
}

pub fn conversation_activity(state: &State) -> Option<ConversationActivitySnapshot> {
	if !available(state) || !state.gateway_connected || state.freshness != Freshness::Fresh {
		return None;
	}
	let id = state.selected?;
	if !state.can_view(id) || !state.can_read_history(id) {
		return None;
	}
	let pins = state
		.search
		.as_ref()
		.filter(|view| view.pins && view.channel == id && !view.loading && view.error.is_none())
		.and_then(|view| view.page.as_ref());
	let pinned_message_ids = pins.map(|page| {
		page.hits
			.iter()
			.filter(|hit| hit.channel == id && hit.id.0 != 0)
			.take(20)
			.map(|hit| hit.id.0.to_string())
			.collect()
	});
	let group = ConversationActivitySnapshot {
		channel_id: id.0.to_string(),
		typing_user_ids: state
			.typing_users(std::time::Instant::now())
			.take(8)
			.filter(|id| id.0 != 0)
			.map(|id| id.0.to_string())
			.collect(),
		pinned_message_ids,
		pins_truncated: pins.is_some_and(|page| {
			page.partial
				|| page.pin_cursor.is_some()
				|| page.hits.len() > 20
				|| page.total > page.hits.len() as u64
		}),
	};
	group.validate().ok()?;
	Some(group)
}

fn phase(phase: client_core::voice::Phase) -> &'static str {
	use client_core::voice::Phase::*;
	match phase {
		Connecting => "connecting",
		ConnectingTransport => "connecting_transport",
		Discovering => "discovering",
		OpeningAudio => "opening_audio",
		Ringing => "ringing",
		Securing => "securing",
		Connected => "connected",
		Waiting => "waiting",
		Failed => "failed",
	}
}

/// One voice roster row as the front end shows it, with mute flags including the own call's
/// live state.
pub struct RosterRow<'a> {
	pub entry: &'a client_core::voice::RosterEntry,
	/// Display name when the member is loaded.
	pub name: Option<&'a str>,
	pub shown: client_core::voice::Participant,
	pub speaking: bool,
}

/// Device-local state a front end owns and plugins may be granted: reading, notification
/// and audio preferences, own presence, and the voice roster as displayed.
pub trait HostLocal {
	fn local_settings(&self) -> LocalSettingsSnapshot;
	fn notification_settings(&self) -> NotificationSettingsSnapshot;
	fn audio_settings(&self) -> AudioSettingsSnapshot;
	fn own_presence(&self) -> OwnPresenceSnapshot;
	/// Whether this device is sharing its screen in the active call.
	fn screen_sharing(&self) -> bool;
	/// Hash of the raw audio and presence preferences, compared by [`ChangeKey`] each frame
	/// without building snapshots.
	fn preferences_key(&self) -> u64;
	/// Calls `each` with viewable roster rows in display order until it returns `false`.
	fn voice_roster(&self, state: &State, each: &mut dyn FnMut(RosterRow<'_>) -> bool);
}

/// Roster rows plugins may see: real ids in voice channels. Viewability is already
/// filtered by [`HostLocal::voice_roster`], like the directory and the call summary.
fn voice_roster(state: &State, local: &dyn HostLocal, each: &mut dyn FnMut(RosterRow<'_>) -> bool) {
	local.voice_roster(state, &mut |row| {
		if row.entry.guild.0 != 0 && row.entry.channel.0 != 0 && row.entry.participant.user.0 != 0 {
			each(row)
		} else {
			true
		}
	});
}

/// Fixed-size change key: no copied messages, user names, or per-frame plugin execution.
#[derive(PartialEq, Eq)]
pub struct ChangeKey {
	channel: Option<Id>,
	connected: bool,
	context: (Freshness, bool, Option<Freshness>),
	voice: Option<VoiceKey>,
	/// Hash of every visible voice channel's people, flags and speaking indicators.
	roster: u64,
	settings: LocalSettingsSnapshot,
	notification_settings: NotificationSettingsSnapshot,
	preferences: u64,
}
#[derive(PartialEq, Eq)]
struct VoiceKey {
	channel: Id,
	request: u64,
	phase: client_core::voice::Phase,
	muted: bool,
	deafened: bool,
	camera: bool,
	streaming: bool,
	participants: u64,
}
impl ChangeKey {
	pub fn capture(state: &State, messaging: &dyn HostLocal) -> Self {
		use std::hash::{Hash, Hasher};
		Self {
			channel: state.selected,
			connected: state.gateway_connected,
			context: (
				state.freshness,
				state.history_pending,
				state
					.members
					.as_ref()
					.filter(|members| Some(members.channel) == state.selected)
					.map(|members| members.freshness),
			),
			roster: {
				let mut roster = std::collections::hash_map::DefaultHasher::new();
				let mut rows = 0;
				voice_roster(state, messaging, &mut |row| {
					(
						row.entry.channel.0,
						row.entry.participant.user.0,
						row.name,
						row.shown.muted,
						row.shown.deafened,
						row.shown.server_muted,
						row.shown.server_deafened,
						row.speaking,
					)
						.hash(&mut roster);
					rows += 1;
					rows < MAX_VOICE_ROSTER
				});
				roster.finish()
			},
			voice: state.voice.active.as_ref().map(|call| {
				let mut participants = std::collections::hash_map::DefaultHasher::new();
				for p in call.participants.iter().take(MAX_VOICE_PARTICIPANTS) {
					(p.user.0, p.muted, p.deafened, p.video, p.streaming).hash(&mut participants);
				}
				VoiceKey {
					channel: call.channel,
					request: call.request,
					phase: call.phase,
					muted: call.muted,
					deafened: call.deafened,
					camera: call.camera,
					streaming: messaging.screen_sharing(),
					participants: participants.finish(),
				}
			}),
			settings: messaging.local_settings(),
			notification_settings: messaging.notification_settings(),
			preferences: messaging.preferences_key(),
		}
	}
	pub fn changed(&self, old: &Self) -> Option<AppEventKind> {
		if self.connected != old.connected {
			Some(AppEventKind::Connection)
		} else if self.channel != old.channel {
			Some(AppEventKind::Navigation)
		} else if self.context != old.context {
			Some(AppEventKind::Context)
		} else if self.voice != old.voice || self.roster != old.roster {
			Some(AppEventKind::Voice)
		} else if self.settings != old.settings
			|| self.notification_settings != old.notification_settings
			|| self.preferences != old.preferences
		{
			Some(AppEventKind::Settings)
		} else {
			None
		}
	}
}
