use extensions::*;

fn plugin(wasm: &str) -> Package {
	Package {
		manifest: Manifest {
			api_version: API_VERSION,
			id: "test-plugin".into(),
			name: "Test plugin".into(),
			version: "1.0.0".into(),
			author: "Serein".into(),
			license: "MIT".into(),
			source: "https://github.com/example/plugin".into(),
			kind: ExtensionKind::Plugin,
			network: Vec::new(),
			secrets: Vec::new(),
			broadcasts: Vec::new(),
			subscribes: Vec::new(),
			capabilities: vec![Capability::Composer],
			actions: vec![Action {
				id: "run".into(),
				label: "Run".into(),
				surface: Surface::Composer,
			}],
		},
		theme: None,
		background_image: Vec::new(),
		cover_image: Vec::new(),
		wasm: wat::parse_str(wasm).unwrap(),
	}
}

fn returning(json: &str) -> Package {
	let data: String = json.bytes().map(|b| format!("\\{b:02x}")).collect();
	plugin(&format!(
		r#"(module (memory (export "memory") 1 256)
		(data (i32.const 32768) "{data}")
		(func (export "serein_alloc") (param i32) (result i32) (i32.const 0))
		(func (export "serein_invoke") (param i32 i32) (result i64) (i64.const {})))"#,
		(32768_u64 << 32) | json.len() as u64
	))
}

fn input() -> Invocation {
	Invocation {
		action: "run".into(),
		composer: Some("hello".into()),
		..Default::default()
	}
}

#[test]
fn rejects_imports_start_and_excessive_memory() {
	assert!(
		plugin(r#"(module (import "wasi_snapshot_preview1" "fd_write" (func)))"#)
			.validate()
			.is_err()
	);
	assert!(
		plugin(r#"(module (func $start) (start $start))"#)
			.validate()
			.is_err()
	);
	let package = plugin(
		r#"(module (memory (export "memory") 257) (func (export "serein_alloc") (param i32) (result i32) i32.const 0) (func (export "serein_invoke") (param i32 i32) (result i64) i64.const 0))"#,
	);
	assert!(invoke(&package, &input()).is_err());
}

#[test]
fn fuel_interrupts_infinite_loop_and_memory_growth_traps() {
	for (body, expected) in [
		("(loop $spin (br $spin)) (i64.const 0)", Error::Fuel),
		(
			"(drop (memory.grow (i32.const 256))) (i64.const 0)",
			Error::Memory,
		),
	] {
		let package = plugin(&format!(
			r#"(module (memory (export "memory") 1) (func (export "serein_alloc") (param i32) (result i32) i32.const 0) (func (export "serein_invoke") (param i32 i32) (result i64) {body}))"#
		));
		let error = invoke(&package, &input()).unwrap_err();
		assert_eq!(
			std::mem::discriminant(&error),
			std::mem::discriminant(&expected)
		);
	}
}

#[test]
fn diagnostics_distinguish_safe_runtime_input_and_output_failures() {
	let trap = plugin(
		r#"(module (memory (export "memory") 1)
        (func (export "serein_alloc") (param i32) (result i32) i32.const 0)
        (func (export "serein_invoke") (param i32 i32) (result i64) unreachable))"#,
	);
	assert!(matches!(invoke(&trap, &input()), Err(Error::Trap)));
	let recursive = plugin(
		r#"(module (memory (export "memory") 1)
        (func (export "serein_alloc") (param i32) (result i32) i32.const 0)
        (func $recurse (result i64) call $recurse i64.const 1 i64.add)
        (func (export "serein_invoke") (param i32 i32) (result i64) call $recurse))"#,
	);
	assert!(matches!(invoke(&recursive, &input()), Err(Error::Stack)));
	assert!(matches!(
		invoke(&returning(""), &input()),
		Err(Error::Handler)
	));
	let private = "private fixture must never appear in diagnostics";
	let malformed = invoke(&returning(private), &input()).unwrap_err();
	assert!(matches!(malformed, Error::Output));
	assert!(!malformed.to_string().contains(private));
	assert!(!format!("{malformed:?}").contains(private));

	let mut request = input();
	request.action = private.into();
	let error = invoke(&returning("{}"), &request).unwrap_err();
	assert!(matches!(error, Error::Input));
	assert!(!error.to_string().contains(private));
	request = input();
	request.composer = Some("x".repeat(MAX_IO_BYTES + 1));
	assert!(matches!(
		invoke(&returning("{}"), &request),
		Err(Error::InputLimit)
	));
	let oversized = plugin(&format!(
		r#"(module (memory (export "memory") 1)
        (func (export "serein_alloc") (param i32) (result i32) i32.const 0)
        (func (export "serein_invoke") (param i32 i32) (result i64) i64.const {}))"#,
		MAX_IO_BYTES + 1
	));
	assert!(matches!(
		invoke(&oversized, &input()),
		Err(Error::OutputLimit)
	));
	let panel = serde_json::json!({"panel": vec![serde_json::json!({"type":"text","text":"x"}); MAX_PANEL_ELEMENTS + 1]});
	assert!(matches!(
		invoke(&returning(&panel.to_string()), &input()),
		Err(Error::OutputLimit)
	));
}

/// Checks that a menu action without its target, or with another kind of target, is
/// rejected as bad input before the handler runs: the handler here traps if it is reached.
#[test]
fn menu_actions_reject_missing_or_mismatched_targets_before_running() {
	let mut trap = plugin(
		r#"(module (memory (export "memory") 1)
        (func (export "serein_alloc") (param i32) (result i32) i32.const 0)
        (func (export "serein_invoke") (param i32 i32) (result i64) unreachable))"#,
	);
	trap.manifest.capabilities = vec![Capability::ContextMenus];
	trap.manifest.actions = vec![Action {
		id: "run".into(),
		label: "Run".into(),
		surface: Surface::User,
	}];
	let user = MenuTarget::User {
		user_id: "4".into(),
		guild_id: None,
		channel_id: None,
	};
	let request = |target: Option<MenuTarget>| Invocation {
		action: "run".into(),
		menu_target: target.map(Box::new),
		..Default::default()
	};
	assert!(matches!(
		invoke(&trap, &request(Some(user))),
		Err(Error::Trap)
	));
	for target in [
		None,
		Some(MenuTarget::Server {
			guild_id: "5".into(),
		}),
	] {
		assert!(matches!(invoke(&trap, &request(target)), Err(Error::Input)));
	}
}

#[test]
fn rejects_oversized_or_invalid_pointers_and_json_responses() {
	for packed in [MAX_IO_BYTES as u64 + 1, (u32::MAX as u64) << 32 | 4] {
		let package = plugin(&format!(
			r#"(module (memory (export "memory") 1) (func (export "serein_alloc") (param i32) (result i32) i32.const 0) (func (export "serein_invoke") (param i32 i32) (result i64) i64.const {packed}))"#
		));
		assert!(invoke(&package, &input()).is_err());
	}
	assert!(invoke(&returning("not json"), &input()).is_err());
	assert!(invoke(&returning(r#"{"send":"not allowed"}"#), &input()).is_err());

	{
		let package = returning(r#"{"replacement":"HELLO","panel":[]}"#);
		let roundtrip = parse_package(&serde_json::to_vec(&package).unwrap()).unwrap();
		for _ in 0..20 {
			assert_eq!(
				invoke(&roundtrip, &input()).unwrap().replacement.as_deref(),
				Some("HELLO")
			);
		}
	}
}

#[test]
fn enforces_capabilities_and_restricts_context_to_action_surface() {
	assert!(matches!(
		invoke(&returning(r#"{"storage":"private"}"#), &input()),
		Err(Error::Capability)
	));
	let mut request = input();
	request.selected_message = Some("not granted".into());
	assert!(matches!(
		invoke(&returning("{}"), &request),
		Err(Error::Capability)
	));
	let mut package = returning(r#"{"replacement":"changed"}"#);
	package.manifest.actions[0].surface = Surface::Panel;
	request = Invocation {
		action: "run".into(),
		..Default::default()
	};
	assert!(matches!(invoke(&package, &request), Err(Error::Capability)));
}

#[test]
fn tick_requires_elapsed_time_and_only_returns_appearance() {
	let mut package = returning(r#"{"appearance":{}}"#);
	package.manifest.capabilities.push(Capability::Appearance);
	package.manifest.actions[0].surface = Surface::Tick;
	let mut request = Invocation {
		action: "run".into(),
		tick_ms: Some(250),
		..Default::default()
	};
	assert!(invoke(&package, &request).is_ok());
	request.tick_ms = None;
	assert!(matches!(invoke(&package, &request), Err(Error::Capability)));
	request.tick_ms = Some(250);
	for response in [
		r#"{"panel":[{"type":"text","text":"no"}]}"#,
		r#"{"storage":"no"}"#,
		r#"{"effects":[] ,"image_sharing":true}"#,
	] {
		package.wasm = returning(response).wasm;
		assert!(matches!(invoke(&package, &request), Err(Error::Capability)));
	}
}

#[test]
fn rejects_panel_complexity_duplicate_ids_and_unknown_actions() {
	let mut package = returning("{}");
	package.manifest.actions.push(Action {
		id: "panel".into(),
		label: "Panel".into(),
		surface: Surface::Panel,
	});
	for panel in [
		vec![Element::Text { text: "x".into() }; MAX_PANEL_ELEMENTS + 1],
		vec![Element::Button {
			id: "missing".into(),
			label: "Button".into(),
		}],
		vec![
			Element::Checkbox {
				id: "same".into(),
				label: "Same".into(),
				checked: false
			};
			2
		],
	] {
		assert!(
			Output {
				panel,
				..Default::default()
			}
			.validate(&package.manifest, &input())
			.is_err()
		);
	}
	let mut tree = vec![Element::Text {
		text: "nested".into(),
	}];
	for _ in 0..10 {
		tree = vec![Element::Row { children: tree }];
	}
	assert!(
		Output {
			panel: tree,
			..Default::default()
		}
		.validate(&package.manifest, &input())
		.is_err()
	);
}

#[test]
fn validates_themes_catalog_and_path_safe_identifiers() {
	for id in ["../test", "CON", "/tmp", "a/b", "", "test.plugin"] {
		assert!(!valid_id(id));
	}
	assert_eq!(parse_color("#11223380").unwrap(), [17, 34, 51, 128]);
	for color in ["red", "#xyzxyz", "#123", "#abcdef😀"] {
		assert!(parse_color(color).is_err());
	}
	let mut theme = Theme::default();
	theme.dark.colors.insert("chat".into(), "#101010".into());
	assert!(theme.validate().is_ok());
	theme.dark.colors.insert("image".into(), "#101010".into());
	assert!(theme.validate().is_err());
	assert!(parse_package(&vec![b' '; MAX_PACKAGE_BYTES + 1]).is_err());
	assert!(parse_catalog(br#"{"api_version":2,"entries":[]}"#).is_err());
	let entry = CatalogEntry {
		description: String::new(),
		preview: None,
		manifest: returning("{}").manifest,
		release_url: "https://example.com/p.json".into(),
		sha256: "a".repeat(64),
		download_bytes: 100,
		source_commit: "a".repeat(40),
		catalog_source: None,
	};
	let catalog = Catalog {
		api_version: API_VERSION,
		entries: vec![entry.clone(), entry],
	};
	assert!(parse_catalog(&serde_json::to_vec(&catalog).unwrap()).is_err());

	{
		let mut theme = Theme::default();
		theme.dark.background = Some(Background {
			opacity: 100,
			sections: Some(SectionOpacity::default()),
			..Default::default()
		});
		let decoded: Theme = serde_json::from_slice(&serde_json::to_vec(&theme).unwrap()).unwrap();
		assert_eq!(decoded, theme);
		assert!(theme.validate().is_ok());
		theme
			.dark
			.background
			.as_mut()
			.unwrap()
			.sections
			.as_mut()
			.unwrap()
			.member_list = 101;
		assert!(theme.validate().is_err());
	}

	{
		let mut package = returning("{}");
		package.cover_image = vec![1];
		assert!(package.validate().is_err());
		package.manifest.kind = ExtensionKind::Theme;
		package.manifest.capabilities.clear();
		package.manifest.actions.clear();
		package.theme = Some(Theme::default());
		package.wasm.clear();
		assert!(package.validate().is_ok());
		package.cover_image.resize(MAX_BACKGROUND_BYTES + 1, 0);
		assert!(package.validate().is_err());
	}
}

#[test]
fn catalog_preview_metadata_is_optional_and_bounded() {
	let mut catalog: serde_json::Value =
		serde_json::from_slice(include_bytes!("../../../extensions/catalog.json")).unwrap();
	for entry in catalog["entries"].as_array_mut().unwrap() {
		entry.as_object_mut().unwrap().remove("description");
		entry.as_object_mut().unwrap().remove("preview");
	}
	let parsed = parse_catalog(&serde_json::to_vec(&catalog).unwrap()).unwrap();
	assert!(
		parsed
			.entries
			.iter()
			.all(|entry| entry.preview.is_none() && entry.description.is_empty())
	);
	catalog["entries"][0]["description"] = serde_json::json!("A preview of the creator's theme.");
	catalog["entries"][0]["preview"] = serde_json::json!({"url": "https://example.org/theme.png", "sha256": "a".repeat(64), "download_bytes": 100});
	assert!(parse_catalog(&serde_json::to_vec(&catalog).unwrap()).is_ok());
	for value in [
		serde_json::json!("http://example.org/preview.png"),
		serde_json::json!("https://name:secret@example.org/preview.png"),
	] {
		let mut invalid = catalog.clone();
		invalid["entries"][0]["preview"]["url"] = value;
		assert!(parse_catalog(&serde_json::to_vec(&invalid).unwrap()).is_err());
	}
	for (field, value) in [
		("sha256", serde_json::json!("invalid")),
		("download_bytes", serde_json::json!(0)),
		(
			"download_bytes",
			serde_json::json!(extensions::MAX_PREVIEW_BYTES + 1),
		),
	] {
		let mut invalid = catalog.clone();
		invalid["entries"][0]["preview"][field] = value;
		assert!(parse_catalog(&serde_json::to_vec(&invalid).unwrap()).is_err());
	}
	for description in ["x".repeat(257), "\u{1F980}".repeat(257), "bad\ntext".into()] {
		let mut invalid = catalog.clone();
		invalid["entries"][0]["description"] = serde_json::json!(description);
		assert!(parse_catalog(&serde_json::to_vec(&invalid).unwrap()).is_err());
	}
}

#[test]
fn image_sharing_plugin_requires_activation_and_capability() {
	let mut package = returning(r#"{"image_sharing":true}"#);
	package.manifest.capabilities = vec![Capability::ImageSharing];
	package.manifest.actions[0].id = "activate".into();
	package.manifest.actions[0].surface = Surface::Activation;
	let input = Invocation {
		action: "activate".into(),
		..Default::default()
	};
	let output = invoke(&package, &input).unwrap();
	assert!(output.image_sharing);
	assert!(output.replacement.is_none() && output.panel.is_empty());
	package.manifest.capabilities.clear();
	assert!(output.validate(&package.manifest, &input).is_err());
	package.manifest.capabilities.push(Capability::ImageSharing);
	package.manifest.actions[0].surface = Surface::Panel;
	assert!(output.validate(&package.manifest, &input).is_err());
	assert!(!serde_json::from_str::<Output>("{}").unwrap().image_sharing);

	{
		let protector = parse_package(include_bytes!(
			"../../../extensions/plugins/packages/message-delete-protector.serein-extension"
		))
		.unwrap();
		let input = Invocation {
			action: "activate".into(),
			..Default::default()
		};
		assert!(
			invoke(&protector, &input).is_ok(),
			"bundled protector package still executes through the ABI"
		);
	}
}

#[test]
fn rgb_cycle_package_executes_through_the_real_abi() {
	let package = parse_package(include_bytes!(
		"../../../examples/extensions/packages/rgb-cycle.serein-extension"
	))
	.unwrap();
	let output = invoke(
		&package,
		&Invocation {
			action: "tick".into(),
			storage: Some(r#"{"base":false}"#.into()),
			tick_ms: Some(250),
			..Default::default()
		},
	)
	.unwrap();
	assert!(!output.appearance.unwrap().dark.colors.contains_key("base"));
}

fn message_event() -> MessageEvent {
	MessageEvent {
		kind: MessageEventKind::Create,
		channel_id: "1".into(),
		message_id: "2".into(),
		author_id: Some("3".into()),
		content: Some("hello".into()),
	}
}

fn event_package(response: &str) -> Package {
	let mut package = returning(response);
	package.manifest.capabilities = vec![Capability::MessageEvents];
	package.manifest.actions[0].surface = Surface::MessageEvent;
	package
}

#[test]
fn message_event_payloads_bound_ids_content_and_partial_fields() {
	let event = message_event();
	event.validate().unwrap();
	for invalid in [
		"",
		"0",
		"000",
		"+1",
		"-1",
		" 1",
		"1.0",
		"18446744073709551616",
		"000000000000000000001",
	] {
		for field in 0..3 {
			let mut invalid_event = event.clone();
			match field {
				0 => invalid_event.channel_id = invalid.into(),
				1 => invalid_event.message_id = invalid.into(),
				_ => invalid_event.author_id = Some(invalid.into()),
			}
			assert!(matches!(invalid_event.validate(), Err(Error::Invalid)));
		}
	}
	let mut event = event;
	event.message_id = u64::MAX.to_string();
	event.content = Some("\u{1F980}".repeat(MAX_EVENT_CONTENT_BYTES / 4));
	event.validate().unwrap();
	event.content.as_mut().unwrap().push('x');
	assert!(matches!(event.validate(), Err(Error::Limit)));
	event.content = None;
	assert!(matches!(event.validate(), Err(Error::Invalid)));
	event.kind = MessageEventKind::Update;
	event.validate().unwrap();
	event.author_id = None;
	event.validate().unwrap();
	event.kind = MessageEventKind::Delete;
	event.validate().unwrap();
	for (author_id, content) in [(Some("3".into()), None), (None, Some(String::new()))] {
		event.author_id = author_id;
		event.content = content;
		assert!(matches!(event.validate(), Err(Error::Invalid)));
	}
}

#[test]
fn message_events_require_a_unique_granted_surface_without_other_context() {
	let mut package = event_package("{}");
	let input = Invocation {
		action: "run".into(),
		message_event: Some(Box::new(message_event())),
		..Default::default()
	};
	package.validate().unwrap();
	invoke(&package, &input).unwrap();
	package.manifest.capabilities.clear();
	assert!(matches!(
		package.manifest.validate(),
		Err(Error::Capability)
	));
	assert!(matches!(
		input.validate(&package.manifest),
		Err(Error::Capability)
	));
	package.manifest.capabilities = vec![
		Capability::MessageEvents,
		Capability::SelectedMessage,
		Capability::Composer,
	];
	package.manifest.actions.push(Action {
		id: "second".into(),
		label: "Second".into(),
		surface: Surface::MessageEvent,
	});
	assert!(matches!(package.manifest.validate(), Err(Error::Invalid)));
	package.manifest.actions.pop();
	for surface in [
		Surface::Panel,
		Surface::Activation,
		Surface::Message,
		Surface::Composer,
	] {
		package.manifest.actions[0].surface = surface;
		assert!(matches!(invoke(&package, &input), Err(Error::Capability)));
	}
	package.manifest.actions[0].surface = Surface::MessageEvent;
	for field in 0..4 {
		let mut invalid = input.clone();
		match field {
			0 => invalid.message_event = None,
			1 => invalid.selected_message = Some("private".into()),
			2 => invalid.composer = Some("draft".into()),
			_ => {
				invalid.values.insert("field".into(), "value".into());
			}
		}
		assert!(invoke(&package, &invalid).is_err());
	}
}

#[test]
fn event_effects_allow_granted_storage_and_appearance_without_unsolicited_ui() {
	let input = Invocation {
		action: "run".into(),
		message_event: Some(Box::new(message_event())),
		..Default::default()
	};
	for response in [r#"{"storage":"1"}"#, r#"{"appearance":{}}"#] {
		let mut package = event_package(response);
		assert!(matches!(invoke(&package, &input), Err(Error::Capability)));
		package
			.manifest
			.capabilities
			.extend([Capability::Storage, Capability::Appearance]);
		invoke(&package, &input).unwrap();
	}
	for response in [
		r#"{"panel":[{"type":"text","text":"unsolicited"}]}"#,
		r#"{"replacement":"unsolicited"}"#,
		r#"{"image_sharing":true}"#,
		r#"{"preserve_deleted_messages":true}"#,
	] {
		let mut package = event_package(response);
		package.manifest.capabilities.extend([
			Capability::Composer,
			Capability::ImageSharing,
			Capability::DeletedMessages,
		]);
		assert!(matches!(invoke(&package, &input), Err(Error::Capability)));
	}
}

/// The published Custom RPC package through the real offline sandbox.
#[test]
fn custom_rpc_package_preserves_drafts_and_controls_presence() {
	let bytes = include_bytes!("../../../extensions/plugins/packages/custom-rpc.serein-extension");
	let package = parse_package(bytes).expect("package validates");
	let mut input = Invocation {
		action: "open".into(),
		..Default::default()
	};
	let opened = invoke(&package, &input).expect("native editor panel validates");
	assert!(opened.rich_presence.is_none());
	assert!(opened.storage.is_none());
	input.values.extend([
		("application-id".into(), "123456789".into()),
		("name".into(), "Synthetic Custom RPC".into()),
		("details".into(), "Offline sandbox check".into()),
		("large-key".into(), "cover".into()),
		("button1-label".into(), "Example".into()),
		("button1-url".into(), "https://example.com".into()),
		("timer".into(), "Custom timestamps".into()),
		("start".into(), "2024-03-01 00:00".into()),
	]);
	input.action = "preview".into();
	let preview = invoke(&package, &input).expect("preview validates");
	assert!(preview.rich_presence.is_none() && preview.storage.is_none());
	assert!(
		preview
			.panel
			.iter()
			.any(|e| matches!(e, Element::ActivityPreview { .. }))
	);
	input.action = "apply".into();
	let applied = invoke(&package, &input).expect("apply validates");
	assert!(matches!(
		applied.rich_presence,
		Some(RichPresenceUpdate::Set { .. })
	));
	input.storage = applied.storage;
	input.values.clear();
	input.action = "activate".into();
	assert!(
		invoke(&package, &input)
			.expect("restore validates")
			.rich_presence
			.is_some()
	);
	input.action = "stop".into();
	let stopped = invoke(&package, &input).expect("stop validates");
	assert!(matches!(
		stopped.rich_presence,
		Some(RichPresenceUpdate::Clear)
	));
	input.storage = stopped.storage;
	input.action = "activate".into();
	assert!(
		invoke(&package, &input)
			.expect("stopped activation validates")
			.rich_presence
			.is_none()
	);
	input.action = "apply".into();
	input
		.values
		.insert("button1-url".into(), "javascript:alert(1)".into());
	let invalid = invoke(&package, &input).expect("invalid field stays a valid editor panel");
	assert!(invalid.rich_presence.is_none() && invalid.storage.is_none());
}

/// Fixed services so import tests are deterministic.
struct Fixed;
impl HostImports for Fixed {
	fn now_ms(&mut self) -> Option<i64> {
		Some(1234)
	}
	fn fill_random(&mut self, out: &mut [u8]) -> bool {
		out.fill(0xab);
		true
	}
	/// Echoes JSON requests; a one-byte request fails with a long message and a two-byte
	/// request succeeds with an oversized reply.
	fn db_execute(&mut self, request: &[u8]) -> Reply {
		match request.len() {
			1 => Reply::Failed("é".repeat(1000)),
			2 => Reply::Ok(vec![b'x'; 300 * 1024]),
			_ => Reply::Ok(request.to_vec()),
		}
	}
	fn journal_read(&mut self, max_entries: u32) -> Reply {
		Reply::Ok(format!(r#"{{"entries":[],"max":{max_entries}}}"#).into_bytes())
	}
}

/// A composer plugin with `capabilities` whose handler runs `body` (leaving an i32 on the
/// stack) and replaces the draft with "ok" when it is non-zero, else "no".
fn importing(imports: &str, capabilities: &[Capability], body: &str) -> Package {
	let ok = r#"{"replacement":"ok"}"#;
	let no = r#"{"replacement":"no"}"#;
	let mut package = plugin(&format!(
		r#"(module {imports} (memory (export "memory") 1)
		(data (i32.const 1024) "{ok}") (data (i32.const 2048) "{no}")
		(func (export "serein_alloc") (param i32) (result i32) (i32.const 4096))
		(func (export "serein_invoke") (param i32 i32) (result i64)
			(if (result i64) {body}
				(then (i64.const {}))
				(else (i64.const {})))))"#,
		(1024_u64 << 32) | ok.len() as u64,
		(2048_u64 << 32) | no.len() as u64,
		ok = ok.replace('"', "\\\""),
		no = no.replace('"', "\\\""),
	));
	package
		.manifest
		.capabilities
		.extend_from_slice(capabilities);
	package
}

const CLOCK: &str = r#"(import "serein" "clock_now_ms" (func $now (result i64)))"#;
const RANDOM: &str = r#"(import "serein" "random_fill" (func $fill (param i32 i32) (result i32)))"#;

const DATABASE: &str = r#"(import "serein" "db_execute" (func $db (param i32 i32) (result i32)))
	(import "serein" "result_read" (func $read (param i32 i32) (result i32)))"#;

fn run(package: &Package, grants: &[Capability]) -> Result<Option<String>, Error> {
	invoke_with(package, &input(), grants, Box::new(Fixed)).map(|output| output.replacement)
}

#[test]
fn host_imports_require_known_signatures_and_declared_capabilities() {
	for imports in [
		r#"(import "env" "clock_now_ms" (func (result i64)))"#,
		r#"(import "serein" "open_file" (func (result i64)))"#,
		r#"(import "serein" "clock_now_ms" (func (result i32)))"#,
		r#"(import "serein" "clock_now_ms" (memory 1))"#,
	] {
		let package = importing(imports, &[Capability::Clock], "(i32.const 1)");
		assert!(
			matches!(package.validate(), Err(Error::Module)),
			"{imports}"
		);
	}
	let undeclared = importing(CLOCK, &[], "(i32.const 1)");
	assert!(matches!(undeclared.validate(), Err(Error::Capability)));
	let declared = importing(
		CLOCK,
		&[Capability::Clock],
		"(i64.eq (call $now) (i64.const 1234))",
	);
	declared.validate().unwrap();
	// Declared but not granted: nothing runs, with or without services.
	assert!(matches!(
		invoke(&declared, &input()),
		Err(Error::Capability)
	));
	assert!(matches!(
		run(&declared, &[Capability::Composer]),
		Err(Error::Capability)
	));
	// A grant the manifest never declared does not unlock an import either.
	let mut stripped = declared.clone();
	stripped
		.manifest
		.capabilities
		.retain(|c| *c != Capability::Clock);
	assert!(matches!(
		run(&stripped, &[Capability::Clock]),
		Err(Error::Capability)
	));
	let grants = [Capability::Composer, Capability::Clock];
	assert_eq!(run(&declared, &grants).unwrap().as_deref(), Some("ok"));
	let unavailable = invoke_with(&declared, &input(), &grants, Box::new(NoHostImports));
	assert_eq!(unavailable.unwrap().replacement.as_deref(), Some("no"));
}

#[test]
fn random_fill_writes_only_bounded_in_range_memory() {
	let grants = [Capability::Composer, Capability::Random];
	let filled = importing(
		RANDOM,
		&[Capability::Random],
		"(i32.and (i32.eqz (call $fill (i32.const 8192) (i32.const 4))) (i32.eq (i32.load (i32.const 8192)) (i32.const 0xabababab)))",
	);
	assert_eq!(run(&filled, &grants).unwrap().as_deref(), Some("ok"));
	// Oversized and negative lengths are refused without writing.
	for length in ["4097", "-1"] {
		let refused = importing(
			RANDOM,
			&[Capability::Random],
			&format!(
				"(i32.and (i32.eq (call $fill (i32.const 8192) (i32.const {length})) (i32.const -1)) (i32.eqz (i32.load (i32.const 8192))))"
			),
		);
		assert_eq!(
			run(&refused, &grants).unwrap().as_deref(),
			Some("ok"),
			"{length}"
		);
	}
	// Writing past the end of linear memory traps instead of touching host memory.
	let out_of_bounds = importing(
		RANDOM,
		&[Capability::Random],
		"(i32.eqz (call $fill (i32.const 65534) (i32.const 4)))",
	);
	assert!(matches!(run(&out_of_bounds, &grants), Err(Error::Trap)));
}

#[test]
fn host_calls_are_budgeted_per_invocation() {
	let spin = importing(
		CLOCK,
		&[Capability::Clock],
		"(loop $again (drop (call $now)) (br $again)) (i32.const 1)",
	);
	let grants = [Capability::Composer, Capability::Clock];
	assert!(matches!(run(&spin, &grants), Err(Error::Fuel)));
	// The budget belongs to one invocation: the next one starts fresh.
	let once = importing(
		CLOCK,
		&[Capability::Clock],
		"(i64.eq (call $now) (i64.const 1234))",
	);
	assert_eq!(run(&once, &grants).unwrap().as_deref(), Some("ok"));
}

#[test]
fn variable_replies_stay_pending_until_read_into_a_large_enough_buffer() {
	let grants = [Capability::Composer, Capability::Database];
	let read_only = r#"(import "serein" "result_read" (func $read (param i32 i32) (result i32)))"#;
	// Reading needs no capability and fails when nothing is pending.
	let nothing = importing(
		read_only,
		&[],
		"(i32.eq (call $read (i32.const 8192) (i32.const 64)) (i32.const -1))",
	);
	nothing.validate().unwrap();
	assert_eq!(
		run(&nothing, &[Capability::Composer]).unwrap().as_deref(),
		Some("ok")
	);
	assert!(matches!(
		importing(DATABASE, &[], "(i32.const 1)").validate(),
		Err(Error::Capability)
	));
	for body in [
		// The 20-byte request at 1024 is echoed; a short buffer keeps it pending, and a read
		// clears it.
		"(i32.and (i32.and (i32.eq (call $db (i32.const 1024) (i32.const 20)) (i32.const 20))
			(i32.eq (call $read (i32.const 8192) (i32.const 19)) (i32.const -1)))
		 (i32.and (i32.and (i32.eq (call $read (i32.const 8192) (i32.const 20)) (i32.const 20))
			(i32.eq (i32.load (i32.const 8192)) (i32.load (i32.const 1024))))
			(i32.eq (call $read (i32.const 8192) (i32.const 20)) (i32.const -1))))",
		// Failure messages are cut to 1 KiB on a character boundary.
		"(i32.and (i32.eq (call $db (i32.const 1024) (i32.const 1)) (i32.const -3))
			(i32.eq (call $read (i32.const 8192) (i32.const 4096)) (i32.const 1024)))",
		// An oversized reply becomes an error, and a new call drops the unread reply.
		"(i32.and (i32.and (i32.eq (call $db (i32.const 1024) (i32.const 2)) (i32.const -3))
			(i32.eq (call $db (i32.const 1024) (i32.const 20)) (i32.const 20)))
			(i32.eq (call $read (i32.const 8192) (i32.const 4096)) (i32.const 20)))",
		// Requests over 256 KiB are refused before memory is read.
		"(i32.eq (call $db (i32.const 0) (i32.const 262145)) (i32.const -1))",
	] {
		let package = importing(DATABASE, &[Capability::Database], body);
		assert_eq!(
			run(&package, &grants).unwrap().as_deref(),
			Some("ok"),
			"{body}"
		);
	}
	let unavailable = importing(
		DATABASE,
		&[Capability::Database],
		"(i32.eq (call $db (i32.const 1024) (i32.const 20)) (i32.const -2))",
	);
	let output = invoke_with(&unavailable, &input(), &grants, Box::new(NoHostImports)).unwrap();
	assert_eq!(output.replacement.as_deref(), Some("ok"));
	// A request that runs past linear memory traps.
	let out_of_bounds = importing(
		DATABASE,
		&[Capability::Database],
		"(i32.eqz (call $db (i32.const 65530) (i32.const 20)))",
	);
	assert!(matches!(run(&out_of_bounds, &grants), Err(Error::Trap)));
}

#[test]
fn journal_actions_need_their_capability_and_return_only_storage() {
	let read = r#"(import "serein" "journal_read" (func $journal (param i32) (result i32)))"#;
	assert!(matches!(
		importing(read, &[], "(i32.const 1)").validate(),
		Err(Error::Capability)
	));
	let grants = [Capability::Composer, Capability::MessageJournal];
	// 1 to 256 entries per call; other counts are refused.
	let bounds = importing(
		read,
		&[Capability::MessageJournal],
		"(i32.and (i32.and (i32.gt_s (call $journal (i32.const 256)) (i32.const 0))
			(i32.eq (call $journal (i32.const 0)) (i32.const -1)))
			(i32.eq (call $journal (i32.const 257)) (i32.const -1)))",
	);
	assert_eq!(run(&bounds, &grants).unwrap().as_deref(), Some("ok"));

	let mut manifest = plugin("(module)").manifest;
	let journal = Action {
		id: "journal".into(),
		label: "Journal".into(),
		surface: Surface::Journal,
	};
	manifest.actions = vec![journal.clone()];
	assert!(matches!(manifest.validate(), Err(Error::Capability)));
	manifest.capabilities.push(Capability::MessageJournal);
	manifest.validate().unwrap();
	let mut twice = manifest.clone();
	twice.actions.push(Action {
		id: "again".into(),
		..journal
	});
	assert!(matches!(twice.validate(), Err(Error::Invalid)));

	let input = Invocation {
		action: "journal".into(),
		..Default::default()
	};
	input.validate(&manifest).unwrap();
	let mut with_values = input.clone();
	with_values.values.insert("x".into(), "1".into());
	assert!(with_values.validate(&manifest).is_err());
	manifest.capabilities.push(Capability::Storage);
	let storage: Output = serde_json::from_str(r#"{"storage":"saved"}"#).unwrap();
	storage.validate(&manifest, &input).unwrap();
	for output in [
		r#"{"replacement":"x"}"#,
		r#"{"panel":[{"type":"text","text":"x"}]}"#,
	] {
		let output: Output = serde_json::from_str(output).unwrap();
		assert!(
			matches!(output.validate(&manifest, &input), Err(Error::Capability)),
			"{output:?}"
		);
	}
}

#[test]
fn network_origins_and_capability_come_together() {
	let request = r#"(import "serein" "https_request" (func $fetch (param i32 i32) (result i32)))"#;
	let mut package = importing(request, &[Capability::Network], "(i32.const 1)");
	// The capability without origins, or origins without the capability, is refused.
	assert!(matches!(package.validate(), Err(Error::Capability)));
	package.manifest.network = vec!["https://api.example.org".into()];
	package.validate().unwrap();
	package
		.manifest
		.capabilities
		.retain(|c| *c != Capability::Network);
	assert!(package.validate().is_err());
	package.manifest.capabilities.push(Capability::Network);
	for origins in [
		vec!["http://api.example.org"],
		vec!["https://api.example.org/path"],
		vec!["https://10.0.0.1"],
		vec!["https://api.example.org", "https://API.example.org/"],
	] {
		package.manifest.network = origins.iter().map(|o| o.to_string()).collect();
		assert!(
			matches!(package.validate(), Err(Error::Invalid)),
			"{origins:?}"
		);
	}
	package.manifest.network = (0..17)
		.map(|i| format!("https://h{i}.example.org"))
		.collect();
	assert!(matches!(package.validate(), Err(Error::Limit)));
}

#[test]
fn topics_are_named_versioned_and_subscribers_declare_a_delivery_action() {
	let mut manifest = plugin("(module)").manifest;
	manifest.broadcasts = vec!["serein.captions/1".into()];
	manifest.validate().unwrap();
	for invalid in [
		vec!["captions"],
		vec!["serein.captions/1", "serein.captions/1"],
	] {
		manifest.broadcasts = invalid.iter().map(|t| t.to_string()).collect();
		assert!(manifest.validate().is_err(), "{invalid:?}");
	}
	manifest.broadcasts.clear();
	// Subscribing needs a topic action, and a topic action needs subscriptions.
	manifest.subscribes = vec!["serein.captions/1".into()];
	assert!(matches!(manifest.validate(), Err(Error::Capability)));
	manifest.actions.push(Action {
		id: "receive".into(),
		label: "Receive".into(),
		surface: Surface::Topic,
	});
	manifest.validate().unwrap();
	manifest.subscribes.clear();
	assert!(matches!(manifest.validate(), Err(Error::Capability)));
	// The imports themselves need no capability; the host checks topics per call.
	let importing_topics = importing(
		r#"(import "serein" "broadcast" (func (param i32 i32) (result i32)))
		(import "serein" "has_subscribers" (func (param i32 i32) (result i32)))
		(import "serein" "topic_read" (func (param i32) (result i32)))"#,
		&[],
		"(i32.const 1)",
	);
	importing_topics.validate().unwrap();
}

#[test]
fn message_hooks_get_only_their_draft_or_batch_and_annotate_only_what_they_were_given() {
	let mut manifest = plugin("(module)").manifest;
	let action = |id: &str, surface| Action {
		id: id.into(),
		label: id.into(),
		surface,
	};
	manifest.capabilities = vec![Capability::SendHook, Capability::MessageAnnotations];
	manifest.actions = vec![action("send", Surface::Send)];
	// A send hook needs its action and the action its capability; annotations alone are a
	// valid grant for `message` actions.
	assert!(matches!(manifest.validate(), Ok(())));
	manifest.capabilities = vec![Capability::MessageAnnotations];
	assert!(matches!(manifest.validate(), Err(Error::Capability)));
	manifest.capabilities = vec![Capability::SendHook, Capability::MessageAnnotations];
	manifest.actions = vec![action("notes", Surface::Annotate)];
	assert!(matches!(manifest.validate(), Err(Error::Capability)));
	manifest.actions.push(action("send", Surface::Send));
	manifest.validate().unwrap();

	let target = || {
		Some(Box::new(SendTarget {
			channel_id: "20".into(),
			guild_id: None,
		}))
	};
	let send = Invocation {
		action: "send".into(),
		composer: Some("hallo".into()),
		send_target: target(),
		..Default::default()
	};
	// The draft is the hook's own grant; no `composer` capability is needed.
	send.validate(&manifest).unwrap();
	for invalid in [
		Invocation {
			send_target: None,
			..send.clone()
		},
		Invocation {
			composer: None,
			..send.clone()
		},
		Invocation {
			action: "notes".into(),
			..send.clone()
		},
	] {
		assert!(invalid.validate(&manifest).is_err());
	}
	let rewrite = Output {
		replacement: Some("hello".into()),
		..Default::default()
	};
	rewrite.validate(&manifest, &send).unwrap();
	let with_panel = Output {
		panel: vec![Element::Separator],
		..rewrite.clone()
	};
	assert!(matches!(
		with_panel.validate(&manifest, &send),
		Err(Error::Capability)
	));

	let message = |id: &str| AnnotateMessage {
		message_id: id.into(),
		author_id: "2".into(),
		content: "hola".into(),
		edited: false,
	};
	let batch = Invocation {
		action: "notes".into(),
		annotate: Some(Box::new(AnnotateBatch {
			channel_id: "20".into(),
			guild_id: None,
			messages: vec![message("1"), message("3")],
		})),
		..Default::default()
	};
	batch.validate(&manifest).unwrap();
	let oversized = Invocation {
		annotate: Some(Box::new(AnnotateBatch {
			channel_id: "20".into(),
			guild_id: None,
			messages: (0..26).map(|id| message(&(id + 1).to_string())).collect(),
		})),
		..batch.clone()
	};
	assert!(matches!(oversized.validate(&manifest), Err(Error::Limit)));
	let note = |id: &str, text: &str| Annotation {
		message_id: id.into(),
		text: text.into(),
	};
	let notes = |annotations| Output {
		annotations,
		..Default::default()
	};
	notes(vec![note("1", "hello"), note("3", "")])
		.validate(&manifest, &batch)
		.unwrap();
	// Only messages the plugin was given, once each, within the size limit.
	for (invalid, error) in [
		(vec![note("2", "hello")], "unrelated message"),
		(vec![note("1", "a"), note("1", "b")], "duplicate"),
		(vec![note("1", &"x".repeat(4097))], "oversized"),
	] {
		assert!(
			notes(invalid).validate(&manifest, &batch).is_err(),
			"{error}"
		);
	}
	assert!(
		notes(vec![note("1", "hello")])
			.validate(&manifest, &send)
			.is_err(),
		"a send hook cannot annotate"
	);
}

#[test]
fn notification_hooks_get_only_their_alert_and_may_only_reword_or_hide_it() {
	let mut manifest = plugin("(module)").manifest;
	manifest.capabilities = vec![Capability::Notifications];
	assert!(matches!(manifest.validate(), Err(Error::Capability)));
	manifest.actions = vec![Action {
		id: "alert".into(),
		label: "Alert".into(),
		surface: Surface::Notification,
	}];
	manifest.validate().unwrap();

	let alert = NotificationInput {
		source: NotificationSource::Gateway,
		message_id: "1".into(),
		channel_id: "20".into(),
		guild_id: None,
		author_id: Some("5".into()),
		direct: true,
		title: "Ada".into(),
		body: "hola".into(),
		upgrade: false,
	};
	let input = Invocation {
		action: "alert".into(),
		notification: Some(Box::new(alert.clone())),
		..Default::default()
	};
	input.validate(&manifest).unwrap();
	for invalid in [
		Invocation {
			notification: None,
			..input.clone()
		},
		Invocation {
			composer: Some("draft".into()),
			..input.clone()
		},
		Invocation {
			notification: Some(Box::new(NotificationInput {
				body: "x".repeat(513),
				..alert.clone()
			})),
			..input.clone()
		},
	] {
		assert!(invalid.validate(&manifest).is_err());
	}

	let decide = |decision: NotificationDecision| Output {
		notification: Some(decision),
		..Default::default()
	};
	decide(NotificationDecision {
		body: Some("hello".into()),
		..Default::default()
	})
	.validate(&manifest, &input)
	.unwrap();
	// Line breaks could fake extra lines in the alert, and hiding takes no text.
	for invalid in [
		NotificationDecision {
			body: Some("hello\nfrom your bank".into()),
			..Default::default()
		},
		NotificationDecision {
			title: Some(" ".into()),
			..Default::default()
		},
		NotificationDecision {
			hide: true,
			body: Some("x".into()),
			..Default::default()
		},
	] {
		assert!(decide(invalid).validate(&manifest, &input).is_err());
	}
	let with_panel = Output {
		panel: vec![Element::Separator],
		..decide(NotificationDecision::default())
	};
	assert!(matches!(
		with_panel.validate(&manifest, &input),
		Err(Error::Capability)
	));

	// The SDK's output decodes into the host's.
	let sdk_output = serde_json::to_value(serein_extension_sdk::NotificationOutput {
		notification: Some(serein_extension_sdk::NotificationDecision {
			hide: true,
			..Default::default()
		}),
		..Default::default()
	})
	.unwrap();
	let output: Output = serde_json::from_value(sdk_output).unwrap();
	output.validate(&manifest, &input).unwrap();
	assert!(output.notification.unwrap().hide);
}

#[test]
fn pages_need_their_grant_and_rows_answer_exactly_the_request() {
	let mut manifest = plugin("(module)").manifest;
	manifest.capabilities = vec![Capability::Pages];
	manifest.actions = vec![
		Action {
			id: "open".into(),
			label: "Open".into(),
			surface: Surface::Panel,
		},
		Action {
			id: "rows".into(),
			label: "Rows".into(),
			surface: Surface::Panel,
		},
	];
	manifest.validate().unwrap();
	let row = |id: &str, text: &str| PageRow {
		id: id.into(),
		author: Some("Ada".into()),
		time_ms: Some(1_700_000_000_000),
		text: text.into(),
		state: Some(PageRowState::Edited),
		revisions: vec![PageRevision {
			time_ms: None,
			text: "first\nversion".into(),
		}],
	};
	let page = |list: PageList| Output {
		page: Some(Some(Page {
			title: "Archive".into(),
			elements: vec![Element::Button {
				id: "open".into(),
				label: "Refresh".into(),
			}],
			list: Some(list),
		})),
		..Default::default()
	};
	let list = PageList {
		version: "v1".into(),
		total: 3,
		rows_action: "rows".into(),
		click_action: None,
		rows: vec![row("1", "hello\nthere")],
		empty_text: None,
	};
	let open = Invocation {
		action: "open".into(),
		..Default::default()
	};
	page(list.clone()).validate(&manifest, &open).unwrap();
	for (invalid, why) in [
		(
			PageList {
				rows_action: "missing".into(),
				..list.clone()
			},
			"undeclared rows action",
		),
		(
			PageList {
				total: 0,
				..list.clone()
			},
			"inline rows past the total",
		),
		(
			PageList {
				rows: vec![row("1", "tab\there")],
				..list.clone()
			},
			"control characters",
		),
		(
			PageList {
				version: String::new(),
				..list.clone()
			},
			"blank version",
		),
	] {
		assert!(page(invalid).validate(&manifest, &open).is_err(), "{why}");
	}
	// Closing needs the grant too, and pages come only from activation and panel actions.
	let close = Output {
		page: Some(None),
		..Default::default()
	};
	close.validate(&manifest, &open).unwrap();
	let mut ungranted = manifest.clone();
	ungranted.capabilities.clear();
	assert!(close.validate(&ungranted, &open).is_err());

	let request = PageRequest {
		version: "v1".into(),
		start: 1,
		count: 2,
	};
	let fetch = Invocation {
		action: "rows".into(),
		page_request: Some(Box::new(request.clone())),
		..Default::default()
	};
	fetch.validate(&manifest).unwrap();
	assert!(fetch.validate(&ungranted).is_err());
	let answer = |version: &str, start, count| Output {
		page_rows: Some(PageRows {
			version: version.into(),
			start,
			rows: (0..count)
				.map(|id: u32| row(&id.to_string(), "x"))
				.collect(),
		}),
		..Default::default()
	};
	answer("v1", 1, 2).validate(&manifest, &fetch).unwrap();
	for (invalid, why) in [
		(answer("v0", 1, 2), "another version"),
		(answer("v1", 0, 2), "another start"),
		(answer("v1", 1, 3), "more rows than asked"),
	] {
		assert!(invalid.validate(&manifest, &fetch).is_err(), "{why}");
	}
	assert!(
		answer("v1", 1, 1).validate(&manifest, &open).is_err(),
		"rows only answer a request"
	);
	let with_panel = Output {
		panel: vec![Element::Separator],
		..answer("v1", 1, 1)
	};
	assert!(
		with_panel.validate(&manifest, &fetch).is_err(),
		"a fetch opens no window"
	);

	// The SDK's `null` closes the page.
	let sdk_close = serde_json::to_value(serein_extension_sdk::PageOutput {
		page: Some(None),
		..Default::default()
	})
	.unwrap();
	let output: Output = serde_json::from_value(sdk_close).unwrap();
	assert!(matches!(output.page, Some(None)));
}

#[test]
fn annotators_add_history_and_deleted_rows_only_around_their_messages_and_captions_need_a_grant() {
	let mut manifest = plugin("(module)").manifest;
	manifest.capabilities = vec![Capability::MessageAnnotations, Capability::CallCaptions];
	manifest.actions = vec![
		Action {
			id: "notes".into(),
			label: "Notes".into(),
			surface: Surface::Annotate,
		},
		Action {
			id: "open".into(),
			label: "Open".into(),
			surface: Surface::Panel,
		},
	];
	manifest.validate().unwrap();
	let message = |id: &str| AnnotateMessage {
		message_id: id.into(),
		author_id: "2".into(),
		content: "hola".into(),
		edited: true,
	};
	let batch = Invocation {
		action: "notes".into(),
		annotate: Some(Box::new(AnnotateBatch {
			channel_id: "20".into(),
			guild_id: None,
			messages: vec![message("10"), message("30")],
		})),
		..Default::default()
	};
	let history = |id: &str| MessageHistory {
		message_id: id.into(),
		revisions: vec![PageRevision {
			time_ms: Some(1),
			text: "earlier\ntext".into(),
		}],
	};
	let deleted = |after: &str, id: &str| DeletedMessages {
		after_message_id: after.into(),
		messages: vec![DeletedMessage {
			message_id: id.into(),
			author: "Ada".into(),
			time_ms: None,
			text: "gone".into(),
		}],
	};
	let output = |histories, deleted_messages| Output {
		histories,
		deleted_messages,
		..Default::default()
	};
	output(vec![history("10")], vec![deleted("10", "20")])
		.validate(&manifest, &batch)
		.unwrap();
	for (invalid, why) in [
		(
			output(vec![history("99")], vec![]),
			"history of a message not given",
		),
		(
			output(vec![history("10"), history("10")], vec![]),
			"duplicate history",
		),
		(
			output(vec![], vec![deleted("99", "20")]),
			"anchor not given",
		),
		(
			output(vec![], vec![deleted("10", "30")]),
			"a loaded message is not deleted",
		),
	] {
		assert!(invalid.validate(&manifest, &batch).is_err(), "{why}");
	}
	let mut too_many = deleted("10", "20");
	too_many.messages = (0..26)
		.map(|id| DeletedMessage {
			message_id: (100 + id).to_string(),
			author: "Ada".into(),
			time_ms: None,
			text: "gone".into(),
		})
		.collect();
	assert!(matches!(
		output(vec![], vec![too_many]).validate(&manifest, &batch),
		Err(Error::Limit)
	));
	let open = Invocation {
		action: "open".into(),
		..Default::default()
	};
	assert!(
		output(vec![history("10")], vec![])
			.validate(&manifest, &open)
			.is_err(),
		"only annotate and message actions add history"
	);

	let captions = |text: &str| Output {
		captions: Some(vec![Caption {
			speaker_id: Some("5".into()),
			text: text.into(),
			done: true,
		}]),
		..Default::default()
	};
	captions("hello").validate(&manifest, &open).unwrap();
	assert!(captions("two\nlines").validate(&manifest, &open).is_err());
	assert!(
		captions("hello").validate(&manifest, &batch).is_err(),
		"hooks do not caption"
	);
	let mut ungranted = manifest.clone();
	ungranted.capabilities = vec![Capability::MessageAnnotations];
	assert!(captions("hello").validate(&ungranted, &open).is_err());
}

#[test]
fn picks_need_the_files_grant_a_declared_action_and_come_from_the_foreground() {
	let mut manifest = plugin("(module)").manifest;
	manifest.capabilities = vec![Capability::Files, Capability::MessageEvents];
	manifest.actions = vec![
		Action {
			id: "picked".into(),
			label: "Picked".into(),
			surface: Surface::Panel,
		},
		Action {
			id: "event".into(),
			label: "Event".into(),
			surface: Surface::MessageEvent,
		},
	];
	manifest.validate().unwrap();
	let pick = |pick: FilePick| Output {
		pick: Some(pick),
		..Default::default()
	};
	let export = FilePick {
		kind: PickKind::SaveFile,
		write: true,
		purpose: "Export the archive".into(),
		name: Some("archive.json".into()),
		action: "picked".into(),
	};
	let panel = Invocation {
		action: "picked".into(),
		..Default::default()
	};
	pick(export.clone()).validate(&manifest, &panel).unwrap();
	for (invalid, why) in [
		(
			FilePick {
				action: "missing".into(),
				..export.clone()
			},
			"undeclared action",
		),
		(
			FilePick {
				kind: PickKind::Folder,
				..export.clone()
			},
			"a name only for save pickers",
		),
		(
			FilePick {
				name: Some("../x.json".into()),
				..export.clone()
			},
			"a name, not a path",
		),
	] {
		assert!(pick(invalid).validate(&manifest, &panel).is_err(), "{why}");
	}
	let mut ungranted = manifest.clone();
	ungranted.capabilities = vec![Capability::MessageEvents];
	assert!(pick(export.clone()).validate(&ungranted, &panel).is_err());
}

#[test]
fn media_actions_pair_with_their_grant_take_bounded_batches_and_only_caption() {
	let mut manifest = plugin("(module)").manifest;
	manifest.capabilities = vec![
		Capability::MediaWorker,
		Capability::CallCaptions,
		Capability::Storage,
	];
	assert!(
		matches!(manifest.validate(), Err(Error::Capability)),
		"grant without action"
	);
	manifest.actions = vec![Action {
		id: "media".into(),
		label: "Media".into(),
		surface: Surface::Media,
	}];
	manifest.validate().unwrap();
	let chunk = |stream: &str, offset, length| MediaItem::Chunk {
		stream: stream.into(),
		codec: "opus".into(),
		time_us: 0,
		offset,
		length,
		rtp_timestamp: Some(960),
		sequence: Some(1),
		keyframe: false,
	};
	let batch = |items: Vec<MediaItem>, data_bytes| Invocation {
		action: "media".into(),
		media: Some(Box::new(MediaBatch {
			seq: 1,
			dropped: 0,
			data_bytes,
			items,
		})),
		..Default::default()
	};
	let input = batch(
		vec![
			MediaItem::Event {
				time_us: 0,
				event: CallEvent::Joined {
					user_id: "5".into(),
				},
			},
			chunk("user:5", 0, 10),
		],
		10,
	);
	input.validate(&manifest).unwrap();
	for (invalid, why) in [
		(
			batch(vec![chunk("user:5", 6, 5)], 10),
			"chunk past the data",
		),
		(
			batch(vec![chunk("user:5", u32::MAX, 2)], 10),
			"overflowing offset",
		),
		(batch(vec![chunk("user 5", 0, 1)], 10), "stream name"),
		(
			batch(vec![], extensions::media::MAX_MEDIA_DATA_BYTES + 1),
			"data size",
		),
		(
			batch(
				(0..257)
					.map(|_| MediaItem::Event {
						time_us: 0,
						event: CallEvent::Ended,
					})
					.collect(),
				0,
			),
			"item count",
		),
		(
			Invocation {
				media: None,
				..input.clone()
			},
			"a media action without a batch",
		),
	] {
		assert!(invalid.validate(&manifest).is_err(), "{why}");
	}
	let captions = Output {
		captions: Some(vec![Caption {
			speaker_id: Some("5".into()),
			text: "hello".into(),
			done: false,
		}]),
		..Default::default()
	};
	captions.validate(&manifest, &input).unwrap();
	for invalid in [
		Output {
			storage: Some("{}".into()),
			..Default::default()
		},
		Output {
			panel: vec![Element::Separator],
			..Default::default()
		},
	] {
		assert!(invalid.validate(&manifest, &input).is_err());
	}

	// Keyframe requests need `media_packets` and name a camera or screen share; the media
	// taps need the worker they run in.
	let keyframes = |streams: &[&str]| Output {
		keyframes: streams.iter().map(|stream| stream.to_string()).collect(),
		..Default::default()
	};
	assert!(
		keyframes(&["camera:5"])
			.validate(&manifest, &input)
			.is_err()
	);
	manifest.capabilities.push(Capability::MediaPackets);
	manifest.validate().unwrap();
	keyframes(&["camera:5", "screen:6"])
		.validate(&manifest, &input)
		.unwrap();
	for invalid in [&["voice:5"][..], &["camera:x"], &["camera"]] {
		assert!(keyframes(invalid).validate(&manifest, &input).is_err());
	}
	let mut unpaired = manifest.clone();
	unpaired.capabilities = vec![Capability::MediaAudio];
	unpaired.actions.clear();
	assert!(matches!(unpaired.validate(), Err(Error::Capability)));
}
