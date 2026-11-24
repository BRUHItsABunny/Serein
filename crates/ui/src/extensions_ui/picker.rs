//! The plugin panel's picker: a search box over the servers, conversations and people Serein
//! has loaded. Names never leave Serein; the plugin gets `<kind>:<id>` for the choice.
use client_core::State;
use extensions::PickerKind;
use model::Id;
use std::collections::{BTreeMap, BTreeSet};

/// Matches listed under the search box.
const MAX_MATCHES: usize = 12;
/// Characters of a search.
const MAX_QUERY: usize = 64;

/// One thing the picker can choose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Choice {
	pub kind: PickerKind,
	pub id: Id,
	pub label: String,
}

impl Choice {
	fn value(&self) -> String {
		format!("{}:{}", self.kind.as_str(), self.id.0)
	}
}

fn kind_label(kind: PickerKind) -> String {
	crate::i18n::translate(match kind {
		PickerKind::Server => "extensions-ui-picker-server",
		PickerKind::Channel => "extensions-ui-picker-channel",
		PickerKind::Dm => "extensions-ui-picker-dm",
		PickerKind::Group => "extensions-ui-picker-group",
		PickerKind::User => "extensions-ui-picker-user",
	})
}

/// Everything of `kinds` Serein has loaded, labelled as the sidebar names it.
pub(crate) fn choices(state: &State, kinds: &[PickerKind]) -> Vec<Choice> {
	let wants = |kind| kinds.contains(&kind);
	let mut out = Vec::new();
	let servers: BTreeMap<Id, &str> = state
		.guilds
		.iter()
		.map(|guild| (guild.id, guild.name.as_str()))
		.collect();
	if wants(PickerKind::Server) {
		out.extend(state.guilds.iter().map(|guild| Choice {
			kind: PickerKind::Server,
			id: guild.id,
			label: guild.name.clone(),
		}));
	}
	let mut people: BTreeMap<Id, String> = BTreeMap::new();
	let own = state.user.as_ref().map(|user| user.id);
	let mut person = |user: &model::User, name: &str| {
		if Some(user.id) != own && user.id.0 != 0 && !user.webhook {
			people.entry(user.id).or_insert_with(|| name.to_owned());
		}
	};
	for channel in &state.channels {
		for user in &channel.recipients {
			person(user, state.user_display_name(user));
		}
		let (kind, label) = match (channel.guild, channel.kind) {
			(None, 1) => (PickerKind::Dm, state.conversation_name(channel).to_owned()),
			(None, 3) => {
				let name = if channel.name.is_empty() {
					channel
						.recipients
						.iter()
						.map(|user| state.user_display_name(user))
						.collect::<Vec<_>>()
						.join(", ")
				} else {
					channel.name.clone()
				};
				(PickerKind::Group, name)
			}
			(Some(guild), _) if channel.supports_text() => {
				let server = servers.get(&guild).copied().unwrap_or_default();
				(PickerKind::Channel, format!("#{} · {server}", channel.name))
			}
			_ => continue,
		};
		if wants(kind) && !label.trim().is_empty() {
			out.push(Choice {
				kind,
				id: channel.id,
				label,
			});
		}
	}
	for message in state.timeline.iter() {
		person(&message.author, state.user_display_name(&message.author));
	}
	for slot in state
		.members
		.iter()
		.flat_map(|list| list.slots.iter().flatten())
	{
		if let model::MemberSlot::Person(member) = slot {
			let name = member
				.nick
				.as_deref()
				.filter(|nick| !nick.is_empty())
				.unwrap_or_else(|| state.user_display_name(&member.user));
			person(&member.user, name);
		}
	}
	if wants(PickerKind::User) {
		out.extend(people.into_iter().map(|(id, label)| Choice {
			kind: PickerKind::User,
			id,
			label,
		}));
	}
	out
}

/// Up to [`MAX_MATCHES`] choices whose name contains every word of `query`, names that start
/// with it first.
pub(crate) fn matches(choices: &[Choice], query: &str) -> Vec<Choice> {
	let query = query.trim().to_lowercase();
	let words: Vec<&str> = query.split_whitespace().collect();
	let mut found: Vec<(bool, &Choice)> = choices
		.iter()
		.filter_map(|choice| {
			let name = choice.label.to_lowercase();
			words
				.iter()
				.all(|word| name.contains(word))
				.then(|| (!name.trim_start_matches('#').starts_with(&query), choice))
		})
		.collect();
	found.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.label.cmp(&b.1.label)));
	let mut seen = BTreeSet::new();
	found
		.into_iter()
		.filter(|(_, choice)| seen.insert((choice.kind, choice.id)))
		.take(MAX_MATCHES)
		.map(|(_, choice)| choice.clone())
		.collect()
}

/// The form key carrying the chosen name next to picker `id`, when it is a valid key.
pub(crate) fn name_key(id: &str) -> Option<String> {
	let key = format!("{id}-name");
	extensions::valid_id(&key).then_some(key)
}

/// Draws picker `id`. The choice goes in `values[id]` and the name shown for it, which the
/// user chose to hand over, in `values["<id>-name"]`.
pub(crate) fn show(
	ui: &mut egui::Ui,
	state: &State,
	id: &str,
	label: &str,
	kinds: &[PickerKind],
	initial: &str,
	values: &mut BTreeMap<String, String>,
) {
	let label = ui.label(label);
	let name_key = name_key(id);
	let value = values
		.entry(id.to_owned())
		.or_insert_with(|| initial.to_owned())
		.clone();
	let query_id = ui.make_persistent_id(("extension-picker", id));
	let mut query: String = ui.data(|data| data.get_temp(query_id)).unwrap_or_default();
	if let Some((kind, raw)) = PickerKind::parse(&value) {
		let known = name_key.as_ref().and_then(|key| values.get(key)).cloned();
		let name = known.unwrap_or_else(|| {
			choices(state, &[kind])
				.into_iter()
				.find(|choice| choice.id.0 == raw)
				.map_or_else(|| raw.to_string(), |choice| choice.label)
		});
		ui.horizontal_wrapped(|ui| {
			ui.label(egui::RichText::new(format!("{}: {name}", kind_label(kind))).strong());
			if ui
				.small_button(crate::i18n::translate("extensions-ui-picker-change"))
				.clicked()
			{
				values.insert(id.to_owned(), String::new());
				if let Some(key) = &name_key {
					values.remove(key);
				}
			}
		});
		return;
	}
	let response = ui
		.add(
			egui::TextEdit::singleline(&mut query)
				.id_salt(("extension-picker-query", id))
				.hint_text(crate::i18n::translate("extensions-ui-picker-hint"))
				.char_limit(MAX_QUERY)
				.desired_width(ui.available_width().min(320.0)),
		)
		.labelled_by(label.id);
	if !query.trim().is_empty() {
		let found = matches(&choices(state, kinds), &query);
		if found.is_empty() {
			ui.weak(crate::i18n::translate("extensions-ui-picker-none"));
		}
		for choice in found {
			let text = format!("{} · {}", choice.label, kind_label(choice.kind));
			if ui
				.push_id(("extension-picker-choice", choice.value()), |ui| {
					ui.selectable_label(false, text)
				})
				.inner
				.clicked()
			{
				values.insert(id.to_owned(), choice.value());
				if let Some(key) = &name_key {
					values.insert(key.clone(), bounded_name(&choice.label));
				}
				query.clear();
			}
		}
	} else if response.has_focus() || kinds.len() > 1 {
		ui.weak(crate::i18n::translate("extensions-ui-picker-help"));
	}
	ui.data_mut(|data| data.insert_temp(query_id, query));
}

/// A name of at most 128 bytes, cut on a character boundary.
fn bounded_name(name: &str) -> String {
	let mut end = name.len().min(128);
	while !name.is_char_boundary(end) {
		end -= 1;
	}
	name[..end].to_owned()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn matches_need_every_word_and_put_name_starts_first() {
		let choice = |kind, id, label: &str| Choice {
			kind,
			id: Id(id),
			label: label.into(),
		};
		let all = vec![
			choice(PickerKind::Channel, 1, "#general · Rust Club"),
			choice(PickerKind::User, 2, "Rusty"),
			choice(PickerKind::Server, 3, "Rust Club"),
			choice(PickerKind::Dm, 4, "Alice"),
		];
		let found = matches(&all, "rust");
		assert_eq!(
			found.iter().map(|c| c.id.0).collect::<Vec<_>>(),
			[3, 2, 1],
			"names starting with the search come first, then the rest"
		);
		assert_eq!(matches(&all, "general club").len(), 1);
		assert!(matches(&all, "bob").is_empty());
	}
}
