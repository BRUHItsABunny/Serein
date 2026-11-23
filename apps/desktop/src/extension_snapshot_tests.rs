//! Snapshot and change-key tests against the desktop `MessagingUi` front end.
use client_core::{State, auth::AuthState};
use extension_host::snapshot::*;
use extensions::*;
use model::{Freshness, Id};
fn manifest(capabilities: Vec<Capability>) -> Manifest {
	Manifest {
		api_version: API_VERSION,
		id: "snapshot-test".into(),
		name: "Synthetic test".into(),
		version: "1".into(),
		author: "Tests".into(),
		license: "MIT".into(),
		source: "https://example.org/source".into(),
		kind: ExtensionKind::Plugin,
		network: Vec::new(),
		secrets: Vec::new(),
		broadcasts: Vec::new(),
		subscribes: Vec::new(),
		capabilities,
		actions: vec![Action {
			id: "show".into(),
			label: "Show".into(),
			surface: Surface::Panel,
		}],
	}
}

/// Regression for #1111: the roster is kept in update order, so on busy accounts the
/// call just joined sat past the cap and plugins never saw it or its speaking changes.
#[test]
fn active_call_survives_a_full_roster() {
	let mut state = test_support::voice_demo_state();
	let other = state
		.voice
		.roster
		.iter()
		.find(|entry| entry.channel == Id(37))
		.unwrap()
		.clone();
	let filler = (0..2 * MAX_VOICE_ROSTER as u64).map(|i| {
		let mut entry = other.clone();
		entry.participant.user = Id(1000 + i);
		entry
	});
	state.voice.roster.splice(0..0, filler);
	let mut messaging = ui::MessagingUi::default();
	let voice = manifest(vec![Capability::VoiceState]);
	let before = ChangeKey::capture(&state, &messaging);
	messaging.voice_speaking = vec![Id(1)];
	let app = snapshot(&state, &messaging, &voice, None).unwrap();
	let you = app
		.voice
		.unwrap()
		.roster
		.into_iter()
		.find(|row| row.user_id == "1")
		.expect("the call stays in a full roster");
	assert!(you.channel_id == "25" && you.speaking);
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&before),
		Some(AppEventKind::Voice)
	);
}

/// Checks that the voice roster lists every viewable voice channel's people with their
/// names and flags, keeps denied channels out, lights speaking only as the client's own
/// indicator does, honours local mutes, and raises a voice event when someone starts
/// talking.
#[test]
fn voice_roster_snapshot_carries_names_flags_and_speaking() {
	let state = test_support::voice_demo_state();
	let mut messaging = ui::MessagingUi::default();
	messaging.voice_speaking = vec![Id(1), Id(2), Id(4)];
	let voice = manifest(vec![Capability::VoiceState]);
	let app = snapshot(&state, &messaging, &voice, None).unwrap();
	let roster = &app.voice.as_ref().unwrap().roster;
	assert_eq!(roster.len(), 5, "every voice channel, not just the call");
	let row = |user: &str| roster.iter().find(|row| row.user_id == user).unwrap();
	assert_eq!(row("1").name.as_deref(), Some("You (synthetic)"));
	assert_eq!(row("1").channel_id, "25");
	assert_eq!(row("4").channel_id, "37");
	assert!(row("2").muted && row("2").deafened);
	assert!(row("3").muted && !row("3").deafened);
	assert!(row("1").speaking, "talking in the call");
	assert!(!row("2").speaking, "muted people never light up");
	assert!(!row("4").speaking, "only the active call reports speaking");
	let before = ChangeKey::capture(&state, &messaging);
	messaging.set_voice_user_mutes(&[1]);
	let app = snapshot(&state, &messaging, &voice, None).unwrap();
	let roster = &app.voice.as_ref().unwrap().roster;
	assert!(
		!roster
			.iter()
			.find(|row| row.user_id == "1")
			.unwrap()
			.speaking,
		"a local mute silences the indicator"
	);
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&before),
		Some(AppEventKind::Voice)
	);
	assert!(
		snapshot(
			&state,
			&messaging,
			&manifest(vec![Capability::AppContext]),
			None
		)
		.unwrap()
		.voice
		.is_none(),
		"roster needs the voice grant"
	);
	// A nickname with control characters is cleaned, an empty one is left out and a long
	// one is bounded, so one bad name never invalidates the whole snapshot.
	let mut state = state;
	let long = "é".repeat(200);
	for (user, nick) in [(4, "Ash\u{0}\u{1b}"), (5, ""), (3, long.as_str())] {
		let entry = state
			.voice
			.roster
			.iter_mut()
			.find(|entry| entry.participant.user == Id(user))
			.unwrap();
		entry.member.as_mut().unwrap().nick = Some(nick.into());
	}
	let app = snapshot(&state, &messaging, &voice, None).expect("bad nicknames keep the snapshot");
	let roster = &app.voice.as_ref().unwrap().roster;
	let row = |user: &str| roster.iter().find(|row| row.user_id == user).unwrap();
	assert_eq!(roster.len(), 5, "every entry still listed");
	assert_eq!(row("4").name.as_deref(), Some("Ash"));
	assert_eq!(row("5").name, None, "an empty nickname is left out");
	let bounded = row("3").name.as_deref().unwrap();
	assert!(bounded.len() <= 128 && long.starts_with(bounded));
	// A channel whose view is denied keeps its people out of the roster, like the
	// directory; only a positive view grant lists a channel.
	let mut permissions = test_support::permission_snapshot(&state);
	for channel in &mut permissions.channels {
		if channel.id == Id(37) {
			channel.overwrites = Some(vec![model::permissions::Overwrite {
				id: Id(10),
				kind: 0,
				allow: 0,
				deny: model::permissions::VIEW_CHANNEL,
			}]);
		}
	}
	state.permissions.replace(permissions).unwrap();
	let roster = &snapshot(&state, &messaging, &voice, None)
		.unwrap()
		.voice
		.unwrap()
		.roster;
	assert_eq!(roster.len(), 3, "denied channels leave the roster");
	assert!(roster.iter().all(|row| row.channel_id == "25"));
	state
		.permissions
		.replace(test_support::permission_snapshot(&state))
		.unwrap();
	// The channel directory keeps the loaded order and carries each channel's visible
	// parent and position for plugins to sort by.
	state.gateway_connected = true;
	let directory = |state: &State, focus| {
		snapshot(
			state,
			&messaging,
			&manifest(vec![Capability::ChannelDirectory]),
			focus,
		)
		.unwrap()
		.channels
		.unwrap()
	};
	let plain = directory(&state, None);
	let visible: Vec<&model::Channel> = state
		.channels
		.iter()
		.filter(|c| state.can_view(c.id))
		.collect();
	assert!(!plain.truncated);
	assert_eq!(plain.items.len(), visible.len());
	for (item, known) in plain.items.iter().zip(&visible) {
		assert_eq!(item.id, known.id.0.to_string(), "loaded order");
		assert_eq!(
			item.parent_id,
			known
				.parent_id
				.filter(|id| state.can_view(*id))
				.map(|id| id.0.to_string())
		);
		assert_eq!(item.position, known.position);
	}
	let room = plain.items.iter().find(|item| item.id == "37").unwrap();
	assert_eq!((room.parent_id.as_deref(), room.position), (Some("24"), 2));
	// Past the shared bounds, the focus server is still listed in full, beyond the
	// shared item cap, and the other servers fill the rest.
	let template = state.channel(Id(21)).unwrap().clone();
	for guild in [200, 201, 202] {
		state.guilds.push(model::Guild {
			banner: None,
			default_message_notifications: None,
			stickers: None,
			id: Id(guild),
			name: format!("Synthetic {guild}"),
			icon: None,
			emojis: None,
		});
		for index in 0..if guild == 202 { 110 } else { 40 } {
			let mut value = template.clone();
			value.id = Id(guild * 1000 + index);
			value.guild = Some(Id(guild));
			value.parent_id = None;
			value.position = index as i32;
			value.name = "c".into();
			state.channels.push(value);
		}
	}
	state
		.permissions
		.replace(test_support::permission_snapshot(&state))
		.unwrap();
	let in_focus = |directory: &ChannelDirectorySnapshot| {
		directory
			.items
			.iter()
			.filter(|item| item.guild_id.as_deref() == Some("202"))
			.count()
	};
	let plain = directory(&state, None);
	assert!(plain.truncated && plain.items.len() <= MAX_APP_CHANNELS);
	assert!(
		in_focus(&plain) < 110,
		"without focus the late server is cut"
	);
	// A dock click picks the focus over the selected dock item, which in turn beats the
	// natively selected server.
	let tree: InterfaceUi = serde_json::from_value(serde_json::json!({
		"nodes": [],
		"dock": [
			{"id": "server:200", "label": "Two hundred", "guild_id": "200", "selected": true},
			{"id": "server:202", "label": "Two hundred two", "guild_id": "202"}
		]
	}))
	.unwrap();
	let click = |id: &str| Invocation {
		action: INTERFACE_CLICK_ACTION.into(),
		values: [("id".into(), id.into())].into(),
		..Default::default()
	};
	let event = Invocation {
		action: "app-event".into(),
		app_event: Some(AppEventKind::Channels),
		..Default::default()
	};
	let focus = focus_guild(&state, &click("server:202"), Some(&tree));
	assert_eq!(
		focus,
		Some(Id(202)),
		"a dock click beats the selected dock item"
	);
	assert_eq!(
		focus_guild(&state, &click("channel:25"), Some(&tree)),
		Some(Id(200)),
		"a row click keeps the selected dock item"
	);
	assert_eq!(focus_guild(&state, &event, Some(&tree)), Some(Id(200)));
	assert_eq!(
		focus_guild(&state, &click("server:202"), None),
		Some(Id(10)),
		"without an interface the natively selected server"
	);
	let focused = directory(&state, focus);
	assert_eq!(in_focus(&focused), 110);
	assert!(
		focused.items[..110]
			.iter()
			.all(|item| item.guild_id.as_deref() == Some("202")),
		"the focus server comes first"
	);
	assert!(focused.truncated, "the other servers still overflow");
	assert!(focused.items.len() - 110 <= MAX_APP_CHANNELS);
	state
		.channels
		.retain(|c| !matches!(c.guild, Some(Id(200 | 201))));
	let focused = directory(&state, focus);
	assert!(!focused.truncated, "everything fits");
	assert_eq!(
		focused.items.len(),
		state
			.channels
			.iter()
			.filter(|c| state.can_view(c.id))
			.count()
	);
}

#[test]
fn extension_app_account_audio_snapshots_are_granted_and_track_preference_changes() {
	let state = test_support::demo_state();
	let mut messaging = ui::MessagingUi::default();
	messaging.voice_processing.profile = model::voice_settings::InputProfile::Studio;
	messaging.own_presence.custom_status = "Synthetic status".into();
	let ungranted = snapshot(
		&state,
		&messaging,
		&manifest(vec![Capability::AppContext]),
		None,
	)
	.unwrap();
	assert!(ungranted.audio_settings.is_none() && ungranted.own_presence.is_none());
	let audio = manifest(vec![Capability::AudioSettings]);
	assert!(uses_app(&audio.capabilities));
	let app = snapshot(&state, &messaging, &audio, None).unwrap();
	assert!(app.own_presence.is_none());
	let settings = app.audio_settings.unwrap();
	assert_eq!(settings.input_profile, "studio");
	assert_eq!(settings.suppression, "off");
	assert_eq!(settings.sensitivity_db, None);
	let account = manifest(vec![Capability::AccountControl]);
	assert!(uses_app(&account.capabilities));
	let app = snapshot(&state, &messaging, &account, None).unwrap();
	assert!(app.audio_settings.is_none());
	assert_eq!(app.own_presence.unwrap().custom_status, "Synthetic status");
	let before = ChangeKey::capture(&state, &messaging);
	messaging.voice_gain.output_percent = 75;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&before),
		Some(AppEventKind::Settings)
	);
	let before = ChangeKey::capture(&state, &messaging);
	messaging.own_presence.custom_status.clear();
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&before),
		Some(AppEventKind::Settings)
	);
	let before = ChangeKey::capture(&state, &messaging);
	messaging.share_game_activity = !messaging.share_game_activity;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&before),
		Some(AppEventKind::Settings)
	);
	messaging.own_presence.custom_status = "x".repeat(129);
	assert!(
		snapshot(&state, &messaging, &account, None)
			.unwrap()
			.own_presence
			.is_none()
	);

	{
		let state = test_support::demo_state();
		let mut messaging = ui::MessagingUi::default();
		let reading = snapshot(
			&state,
			&messaging,
			&manifest(vec![Capability::LocalSettings]),
			None,
		)
		.unwrap();
		assert!(reading.notification_settings.is_none());
		assert_eq!(reading.settings.unwrap().scroll_speed_percent, Some(100));
		let caps = manifest(vec![Capability::NotificationSettings]);
		assert!(uses_app(&caps.capabilities));
		let app = snapshot(&state, &messaging, &caps, None).unwrap();
		assert!(app.settings.is_none() && app.context.is_none());
		assert_eq!(
			app.notification_settings.unwrap(),
			messaging.extension_notification_settings()
		);
		let before = ChangeKey::capture(&state, &messaging);
		messaging.notification_options.volume = 10;
		assert_eq!(
			ChangeKey::capture(&state, &messaging).changed(&before),
			Some(AppEventKind::Settings)
		);
		let mut preferences = crate::app_settings::Settings::default();
		preferences.observe(&messaging);
		assert!(preferences.state.dirty);
		let mut restored = ui::MessagingUi::default();
		preferences.apply(&mut restored);
		assert_eq!(
			restored.notification_options,
			messaging.notification_options
		);
		let before = ChangeKey::capture(&state, &messaging);
		messaging.reading_preferences.scroll_speed_percent = 150;
		assert_eq!(
			ChangeKey::capture(&state, &messaging).changed(&before),
			Some(AppEventKind::Settings)
		);
	}
}

#[test]
fn extension_app_message_metadata_and_relationships_are_scoped_and_bounded() {
	let mut state = test_support::demo_state();
	let messaging = ui::MessagingUi::default();
	let caps = manifest(vec![Capability::MessageDetails, Capability::Relationships]);
	let mut message = test_support::message(9000, Id(20));
	message.reply_to = Some(Id(8999));
	message.mentions = (1..=40)
		.map(|id| model::User {
			id: Id(id),
			..state.user.clone().unwrap()
		})
		.collect();
	message.attachments = vec![model::Attachment {
		id: Id(70),
		filename: "report.txt".into(),
		description: Some("PRIVATE_DESCRIPTION".into()),
		content_type: Some("text/plain".into()),
		size: 123,
		media: model::EmbedMedia {
			url: Some("https://example.org/PRIVATE_URL".into()),
			..Default::default()
		},
		spoiler: true,
		duration_ms: None,
		waveform: vec![],
	}];
	message.reactions = Some(
		(0..20)
			.map(|id| model::Reaction {
				emoji: model::ReactionEmoji {
					id: Some(Id(100 + id)),
					name: Some("test".into()),
				},
				count: 2,
				me: true,
				me_burst: false,
			})
			.collect(),
	);
	let detail = message_detail(&message);
	assert_eq!(detail.mention_ids.len(), MAX_MESSAGE_MENTIONS);
	assert!(detail.mentions_truncated && detail.reactions_truncated);
	assert!(detail.reactions.as_ref().unwrap().len() <= MAX_MESSAGE_REACTIONS);
	assert_eq!(detail.attachments[0].filename, "report.txt");
	assert_eq!(detail.reply_to.as_deref(), Some("8999"));
	let wire = serde_json::to_string(&detail).unwrap();
	assert!(!wire.contains("PRIVATE_") && !wire.contains(&message.content));
	let mut malformed = message.clone();
	malformed.attachments[0].filename = "\n\t".into();
	malformed.attachments[0].content_type = Some("\n".into());
	malformed.attachments.push(malformed.attachments[0].clone());
	let sanitized = message_detail(&malformed);
	assert_eq!(sanitized.attachments.len(), 1);
	assert_eq!(sanitized.attachments[0].filename, "Attachment");
	assert!(sanitized.attachments[0].content_type.is_none() && sanitized.attachments_truncated);
	AppSnapshot {
		message_details: Some(MessageDetailsSnapshot {
			channel_id: "20".into(),
			items: vec![sanitized],
			truncated: false,
		}),
		..Default::default()
	}
	.validate(&caps)
	.unwrap();
	message.mentions.clear();
	state
		.timeline
		.insert(message.clone(), false, false)
		.unwrap();
	let mut private = message.clone();
	private.id = Id(9001);
	private.ephemeral = true;
	private.flags |= 64;
	state.timeline.insert(private, false, false).unwrap();
	state.set_preserve_deleted_messages(true);
	let mut deleted = message;
	deleted.id = Id(9002);
	state.timeline.insert(deleted, false, false).unwrap();
	state.apply(client_core::Envelope {
		generation: state.generation,
		event: client_core::Event::Delete {
			channel: Id(20),
			id: Id(9002),
		},
	});
	let person = |id| model::User {
		id: Id(id),
		..state.user.clone().unwrap()
	};
	let people: Vec<_> = (800..805).map(person).collect();
	use client_core::user_actions::Event as U;
	for event in [
		U::Relationships(Some(vec![
			(Id(800), false),
			(Id(801), false),
			(Id(802), false),
			(Id(803), true),
			(Id(804), false),
		])),
		U::Friends(Some(vec![(people[0].clone(), "friend".into())])),
		U::Requests(Some(vec![
			(people[1].clone(), "incoming".into(), true),
			(people[2].clone(), "outgoing".into(), false),
		])),
		U::Restrictions(Some(vec![
			(people[3].clone(), "blocked".into(), false),
			(people[4].clone(), "ignored".into(), true),
		])),
	] {
		state.apply(client_core::Envelope {
			generation: state.generation,
			event: client_core::Event::UserAction(event),
		});
	}
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert!(app.bytes().unwrap() <= MAX_APP_SNAPSHOT_BYTES);
	let details = app.message_details.as_ref().unwrap();
	assert!(details.items.iter().any(|m| m.id == "9000"));
	assert!(
		!details
			.items
			.iter()
			.any(|m| matches!(m.id.as_str(), "9001" | "9002"))
	);
	let relationships = app.relationships.as_ref().unwrap();
	assert!(
		relationships.friends_known
			&& relationships.requests_known
			&& relationships.restricted_known
	);
	assert_eq!(relationships.items.len(), 5);
	assert_eq!(relationships.items[0].kind, RelationshipKind::Friend);
	assert_eq!(relationships.items[4].kind, RelationshipKind::Ignored);
	let no_grants = snapshot(
		&state,
		&messaging,
		&manifest(vec![Capability::AppContext]),
		None,
	)
	.unwrap();
	assert!(no_grants.message_details.is_none() && no_grants.relationships.is_none());
	state.freshness = Freshness::Stale;
	assert!(
		snapshot(&state, &messaging, &caps, None)
			.unwrap()
			.message_details
			.is_none()
	);
	state.gateway_connected = false;
	let disconnected = snapshot(&state, &messaging, &caps, None).unwrap();
	assert!(disconnected.message_details.is_none() && disconnected.relationships.is_none());
}
#[test]
fn extension_app_new_data_requires_grants_and_stays_bounded_and_current() {
	let mut state = test_support::demo_state();
	let messaging = ui::MessagingUi::default();
	let current = state.user.clone().unwrap();
	state.own_profile.data = Some(model::UserProfile {
		user: current.clone(),
		username: current.name.clone(),
		global_name: Some("Synthetic display".into()),
		banner: None,
		accent_color: None,
		bio: "\\".repeat(4096),
		pronouns: "they/them".into(),
		badges: vec![],
		connections: vec![],
		mutual_guilds: vec![],
		mutual_friends: vec![],
		guild: None,
		theme_colors: None,
		clan: None,
		board: None,
		effect_sku: None,
		frame_sku: None,
		effect: None,
		frame: None,
		badge_catalog: None,
		private: false,
		limited: false,
	});
	let guild = state.guilds[0].clone();
	state.guilds.extend((1..=150).map(|id| model::Guild {
		id: Id(10000 + id),
		name: "Synthetic server ".repeat(20),
		..guild.clone()
	}));
	let friends = (1000..1150)
		.map(|id| {
			(
				model::User {
					id: Id(id),
					..current.clone()
				},
				format!("friend{id}"),
			)
		})
		.collect();
	for event in [
		client_core::user_actions::Event::Relationships(Some(
			(1000..1150).map(|id| (Id(id), false)).collect(),
		)),
		client_core::user_actions::Event::Friends(Some(friends)),
	] {
		state.apply(client_core::Envelope {
			generation: state.generation,
			event: client_core::Event::UserAction(event),
		});
	}
	let selected = state.selected.unwrap();
	state
		.channels
		.iter_mut()
		.find(|c| c.id == selected)
		.unwrap()
		.recipients = (1..=50)
		.map(|id| model::User {
			id: Id(20000 + id),
			name: "Synthetic recipient ".repeat(20),
			..current.clone()
		})
		.collect();
	let caps = manifest(vec![
		Capability::AccountProfile,
		Capability::MessageDetails,
		Capability::Relationships,
		Capability::GuildDirectory,
		Capability::ChannelDetails,
		Capability::AppContext,
		Capability::ChannelDirectory,
		Capability::Timeline,
		Capability::Members,
		Capability::Presence,
		Capability::VoiceState,
		Capability::ReadState,
		Capability::LocalSettings,
		Capability::NotificationSettings,
	]);
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert!(app.relationships.as_ref().unwrap().truncated);
	assert!(app.message_details.is_some());

	let account = app.account_profile.as_ref().unwrap();
	assert_eq!(account.user.id, current.id.0.to_string());
	assert_eq!(account.profile.as_ref().unwrap().bio.len(), 2048);
	assert!(app.guilds.as_ref().unwrap().truncated);
	assert!(app.guilds.as_ref().unwrap().items.len() <= MAX_APP_GUILDS);
	let details = app.channel_details.as_ref().unwrap();
	assert!(details.recipients_truncated && details.recipients.len() <= MAX_CHANNEL_RECIPIENTS);
	assert!(app.bytes().unwrap() <= MAX_APP_SNAPSHOT_BYTES);
	let ungranted = snapshot(
		&state,
		&messaging,
		&manifest(vec![Capability::AppContext]),
		None,
	)
	.unwrap();
	assert!(
		ungranted.account_profile.is_none()
			&& ungranted.guilds.is_none()
			&& ungranted.channel_details.is_none()
	);
	state.own_profile.data.as_mut().unwrap().user.id = Id(999);
	assert!(
		snapshot(&state, &messaging, &caps, None)
			.unwrap()
			.account_profile
			.as_ref()
			.unwrap()
			.profile
			.is_none()
	);
	state.freshness = Freshness::Unavailable;
	assert!(
		snapshot(&state, &messaging, &caps, None)
			.unwrap()
			.channel_details
			.is_none()
	);
	state.gateway_connected = false;
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert!(app.account_profile.is_none() && app.guilds.is_none() && app.channel_details.is_none());
}

#[test]
fn extension_activity_distinguishes_unloaded_pins_and_live_typing() {
	let mut state = test_support::demo_state();
	state.demo = false;
	state.auth = AuthState::Authenticated;
	state.history_pending = false;
	let channel = state.selected.unwrap();
	let now = std::time::Instant::now();
	let wall = std::time::SystemTime::now();
	state.observe_typing_at(
		client_core::typing::Signal {
			channel,
			user: Id(9876),
			timestamp: wall
				.duration_since(std::time::UNIX_EPOCH)
				.unwrap()
				.as_secs(),
		},
		wall,
		now,
	);
	let activity = conversation_activity(&state).unwrap();
	assert_eq!(activity.typing_user_ids, vec!["9876"]);
	assert!(activity.pinned_message_ids.is_none());
	let before_pins = extension_host::data_events::DataKey::capture(&state);
	state.search = Some(client_core::search::SearchView {
		pins: true,
		channel,
		guild: None,
		query: String::new(),
		before: None,
		oldest: false,
		offset: 0,
		total: None,
		pin_before: None,
		request: 0,
		loading: false,
		error: None,
		page: Some(model::SearchPage {
			hits: vec![],
			total: 0,
			partial: false,
			pin_cursor: None,
		}),
	});
	assert!(
		extension_host::data_events::DataKey::capture(&state)
			.changed(&before_pins)
			.kinds(&[Capability::ConversationActivity])
			.any(|kind| kind == AppEventKind::Pins)
	);
	assert_eq!(
		conversation_activity(&state).unwrap().pinned_message_ids,
		Some(vec![])
	);
	state.search.as_mut().unwrap().loading = true;
	assert!(
		conversation_activity(&state)
			.unwrap()
			.pinned_message_ids
			.is_none()
	);
	let inspector = parse_package(include_bytes!(
		"../../../examples/extensions/packages/conversation-inspector.serein-extension"
	))
	.unwrap();
	let app = snapshot(
		&state,
		&ui::MessagingUi::default(),
		&inspector.manifest,
		None,
	)
	.unwrap();
	assert!(
		app.message_content.is_some()
			&& app.forum_data.is_some()
			&& app.conversation_activity.is_some()
	);
	let output = invoke(
		&inspector,
		&Invocation {
			action: "show".into(),
			app: Some(app),
			..Default::default()
		},
	)
	.expect("real loaded state fits Conversation Inspector sandbox");
	assert!(!output.panel.is_empty() && output.effects.is_empty());
	let granted = snapshot(
		&state,
		&ui::MessagingUi::default(),
		&manifest(vec![Capability::ConversationActivity]),
		None,
	)
	.unwrap();
	assert!(granted.conversation_activity.is_some());
	assert!(
		snapshot(
			&state,
			&ui::MessagingUi::default(),
			&manifest(vec![Capability::AppContext]),
			None,
		)
		.unwrap()
		.conversation_activity
		.is_none()
	);
}

#[test]
fn extension_channel_metadata_exposes_only_loaded_topics_and_scoped_parents() {
	use client_core::channel_actions::{Action, Edit, Event, Outcome};
	let mut state = test_support::demo_state();
	let id = state.selected.unwrap();
	let original = state.channel(id).unwrap().clone();
	let guild = original.guild.unwrap();
	let mut category = original.clone();
	category.id = Id(88001);
	category.kind = 4;
	category.name = "Category".into();
	category.parent_id = None;
	state.channels.push(category);
	state.permissions.channels.insert(
		Id(88001),
		model::permissions::Channel {
			id: Id(88001),
			guild,
			overwrites: Some(vec![]),
		},
	);
	state.permissions.guilds.get_mut(&guild).unwrap().owner =
		state.user.as_ref().map(|user| user.id);
	state.permissions.clear_cache();
	let index = state.channel_index(id).unwrap();
	state.channels[index].parent_id = Some(Id(88001));
	let unknown = channel_metadata(&state).unwrap();
	assert!(unknown.topic.is_none() && unknown.thread.is_none());
	assert_eq!(unknown.category.as_ref().unwrap().name, "Category");
	let client_core::Command::ChannelAction { request, .. } =
		state.request_channel_action(id, Action::Load).unwrap()
	else {
		panic!("expected synthetic load command")
	};
	state.apply(client_core::Envelope {
		generation: state.generation,
		event: client_core::Event::ChannelAction(Event::Finished {
			guild,
			channel: id,
			request,
			result: Ok(Outcome::Details(Edit {
				name: original.name,
				topic: "\u{e9}".repeat(1200),
				slowmode: 5,
				nsfw: false,
				overwrites: vec![],
				forum: None,
			})),
		}),
	});
	let loaded = channel_metadata(&state).unwrap();
	loaded.validate().unwrap();
	assert_eq!(loaded.topic.as_ref().unwrap().len(), 2048);
	assert!(loaded.topic_truncated);
	assert_eq!(loaded.slowmode_seconds, Some(5));
	let messaging = ui::MessagingUi::default();
	let granted = snapshot(
		&state,
		&messaging,
		&manifest(vec![Capability::ChannelMetadata]),
		None,
	)
	.unwrap();
	assert!(granted.channel_metadata.is_some());
	let inspector = parse_package(include_bytes!(
		"../../../examples/extensions/packages/guild-inspector.serein-extension"
	))
	.unwrap();
	let output = invoke(
		&inspector,
		&Invocation {
			action: "show".into(),
			app: Some(granted),
			..Default::default()
		},
	)
	.expect("loaded channel metadata fits the real Guild Inspector sandbox");
	assert!(!output.panel.is_empty() && output.effects.is_empty());
	let denied = snapshot(
		&state,
		&messaging,
		&manifest(vec![Capability::ChannelDetails]),
		None,
	)
	.unwrap();
	assert!(denied.channel_metadata.is_none());
	state.demo = false;
	state.auth = AuthState::Authenticated;
	let before_reload = extension_host::data_events::DataKey::capture(&state);
	state.request_channel_action(id, Action::Load).unwrap();
	assert!(channel_metadata(&state).unwrap().topic.is_none());
	assert!(
		extension_host::data_events::DataKey::capture(&state)
			.changed(&before_reload)
			.kinds(&[Capability::ChannelMetadata])
			.any(|kind| kind == AppEventKind::Channels)
	);
	let mut parent = state.channel(id).unwrap().clone();
	parent.id = Id(88002);
	state.channels.push(parent);
	state.permissions.channels.insert(
		Id(88002),
		model::permissions::Channel {
			id: Id(88002),
			guild,
			overwrites: Some(vec![]),
		},
	);
	state.channels[index].kind = 11;
	state.channels[index].parent_id = Some(Id(88002));
	state.channels[index].message_count = Some(7);
	let thread = channel_metadata(&state).unwrap();
	assert_eq!(thread.category.as_ref().unwrap().id, "88001");
	let thread = thread.thread.unwrap();
	assert_eq!(thread.message_count, Some(7));
	assert!(thread.archived.is_none() && thread.locked.is_none());
	state.channels.last_mut().unwrap().guild = Some(Id(999));
	assert!(channel_metadata(&state).is_none());
}

#[test]
fn extension_app_snapshot_is_granted_current_and_byte_bounded() {
	let mut state = test_support::demo_state();
	let messaging = ui::MessagingUi::default();
	assert!(
		snapshot(
			&state,
			&messaging,
			&manifest(vec![Capability::Storage]),
			None
		)
		.is_none()
	);
	let caps = manifest(vec![
		Capability::AccountProfile,
		Capability::MessageDetails,
		Capability::Relationships,
		Capability::GuildDirectory,
		Capability::ChannelDetails,
		Capability::AppContext,
		Capability::Timeline,
		Capability::ChannelDirectory,
		Capability::Members,
		Capability::Presence,
		Capability::VoiceState,
		Capability::ReadState,
		Capability::LocalSettings,
		Capability::NotificationSettings,
	]);
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert_eq!(
		app.context.as_ref().unwrap().channel.as_ref().unwrap().id,
		"20"
	);
	assert!(app.bytes().unwrap() <= MAX_APP_SNAPSHOT_BYTES);
	assert_eq!(
		app.timeline.as_ref().unwrap().messages.len(),
		PAIRED_MESSAGE_ROWS
	);
	assert_eq!(
		app.message_details.as_ref().unwrap().items.len(),
		PAIRED_MESSAGE_ROWS
	);
	assert!(app.message_details.as_ref().unwrap().truncated);
	assert!(app.timeline.as_ref().unwrap().truncated);
	let toolbox = parse_package(include_bytes!(
		"../../../examples/extensions/packages/app-toolbox.serein-extension"
	))
	.unwrap();
	let output = invoke(
		&toolbox,
		&Invocation {
			action: "show".into(),
			app: Some(app),
			..Default::default()
		},
	)
	.expect("the real demo snapshot must fit App Toolbox's sandbox fuel budget");
	assert!(!output.panel.is_empty() && output.effects.is_empty());
	let app = snapshot(
		&state,
		&messaging,
		&manifest(vec![Capability::LocalSettings]),
		None,
	)
	.unwrap();
	assert!(app.context.is_none() && app.channels.is_none() && app.timeline.is_none());
	assert!(app.settings.is_some());
	let member = model::Member {
		user: state.user.clone().unwrap(),
		roles: Vec::new(),
		nick: None,
		status: Some("online".into()),
		custom_status: None,
		clients: model::ClientPlatforms::default(),
		activities: Vec::new(),
	};
	state.members = Some(model::MemberList {
		guild: Some(Id(10)),
		channel: Id(20),
		request: state.member_request,
		start: 0,
		slots: vec![
			Some(model::MemberSlot::Person(member.clone())),
			Some(model::MemberSlot::Person(member)),
		],
		lazy: false,
		groups: vec![],
		ranges: vec![],
		total: 2,
		freshness: Freshness::Loading,
	});
	let loading = ChangeKey::capture(&state, &messaging);
	state.members.as_mut().unwrap().freshness = Freshness::Fresh;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&loading),
		Some(AppEventKind::Context)
	);
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert_eq!(app.members.as_ref().unwrap().items.len(), 1);
	assert_eq!(app.presence.as_ref().unwrap().items.len(), 1);
	state.freshness = Freshness::Unavailable;
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert!(app.context.as_ref().unwrap().channel.is_none());
	assert!(app.members.is_none() && app.presence.is_none() && app.timeline.is_none());
	assert!(
		app.channels
			.as_ref()
			.unwrap()
			.items
			.iter()
			.all(|channel| channel.id != "20")
	);
	state.gateway_connected = false;
	let app = snapshot(&state, &messaging, &caps, None).unwrap();
	assert!(!app.context.as_ref().unwrap().connected);
	assert!(
		app.channels.is_none()
			&& app.timeline.is_none()
			&& app.members.is_none()
			&& app.presence.is_none()
	);
	state.demo = false;
	state.auth = AuthState::Unauthenticated;
	assert!(snapshot(&state, &messaging, &caps, None).is_none());
}
#[test]
fn extension_app_timeline_excludes_private_deleted_and_oversized_rows() {
	let mut state = test_support::demo_state();
	state.set_preserve_deleted_messages(true);
	let caps = manifest(vec![Capability::Timeline]);
	for (id, text, ephemeral) in [
		(2001, "private".into(), true),
		(2002, "x".repeat(4097), false),
		(2003, "removed".into(), false),
		(2004, "visible".into(), false),
	] {
		let mut message = test_support::message(id, Id(20));
		message.content = text;
		message.ephemeral = ephemeral;
		state.apply(client_core::Envelope {
			generation: state.generation,
			event: client_core::Event::Message(message),
		});
	}
	state.apply(client_core::Envelope {
		generation: state.generation,
		event: client_core::Event::Delete {
			channel: Id(20),
			id: Id(2003),
		},
	});
	let app = snapshot(&state, &ui::MessagingUi::default(), &caps, None).unwrap();
	let timeline = app.timeline.as_ref().unwrap();
	assert!(timeline.messages.iter().any(|m| m.id == "2004"));
	assert!(
		timeline
			.messages
			.iter()
			.all(|m| !["2001", "2002", "2003"].contains(&m.id.as_str()))
	);
	assert!(timeline.truncated);
	state.freshness = Freshness::Unavailable;
	assert!(
		snapshot(&state, &ui::MessagingUi::default(), &caps, None)
			.unwrap()
			.timeline
			.is_none()
	);
}
#[test]
fn extension_app_change_key_is_idle_until_meaningful_change() {
	let mut state = test_support::demo_state();
	let mut messaging = ui::MessagingUi::default();
	let first = ChangeKey::capture(&state, &messaging);
	state.revision += 1;
	assert_eq!(ChangeKey::capture(&state, &messaging).changed(&first), None);
	state.history_pending = !state.history_pending;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&first),
		Some(AppEventKind::Context)
	);
	state.history_pending = !state.history_pending;
	state.freshness = Freshness::Loading;
	let loading = ChangeKey::capture(&state, &messaging);
	state.freshness = Freshness::Fresh;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&loading),
		Some(AppEventKind::Context)
	);
	messaging.reading_preferences.zoom_percent = 110;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&first),
		Some(AppEventKind::Settings)
	);
	state.selected = None;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&first),
		Some(AppEventKind::Navigation)
	);
	state.gateway_connected = false;
	assert_eq!(
		ChangeKey::capture(&state, &messaging).changed(&first),
		Some(AppEventKind::Connection)
	);
}
