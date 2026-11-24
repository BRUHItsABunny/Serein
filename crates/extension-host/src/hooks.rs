//! Message hooks the user switched on per channel, the notes plugins add under messages, and
//! which messages of the open channel are due for a plugin's `annotate` action.
use client_core::State;
use extensions::{
	AnnotateBatch, AnnotateMessage, MAX_EVENT_CONTENT_BYTES,
	message_hooks::{MAX_ANNOTATE_BYTES, MAX_ANNOTATE_MESSAGES, MAX_ANNOTATION_BYTES},
};
use model::{Id, Message};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Channels with saved hooks in one account.
pub const MAX_CHANNELS: usize = 512;
/// Plugins annotating one channel.
pub const MAX_ANNOTATORS: usize = 4;
/// Latest loaded messages of the open channel considered for annotation.
const ANNOTATE_WINDOW: usize = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendMode {
	/// The rewrite replaces the draft, and the user sends it after reviewing.
	#[default]
	Preview,
	/// The rewrite is sent at once.
	Auto,
}

/// What happens to the draft when the hook fails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
	/// Nothing is sent; the draft stays in the composer.
	#[default]
	KeepDraft,
	/// The draft is sent unchanged.
	SendOriginal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendHook {
	pub plugin: String,
	#[serde(default)]
	pub mode: SendMode,
	#[serde(default)]
	pub fallback: Fallback,
}

/// The hooks switched on in one channel.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelHooks {
	pub channel: Id,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub send: Option<SendHook>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub annotate: Vec<String>,
}

impl ChannelHooks {
	pub fn is_empty(&self) -> bool {
		self.send.is_none() && self.annotate.is_empty()
	}

	/// The plugins these hooks run.
	pub fn plugins(&self) -> impl Iterator<Item = &str> {
		self.send
			.iter()
			.map(|hook| hook.plugin.as_str())
			.chain(self.annotate.iter().map(String::as_str))
	}

	pub fn validate(&self) -> Result<(), &'static str> {
		let mut seen = std::collections::BTreeSet::new();
		if self.channel.0 == 0
			|| (self.channel == EVERYWHERE && self.send.is_some())
			|| self.annotate.len() > MAX_ANNOTATORS
			|| !self.annotate.iter().all(|plugin| seen.insert(plugin))
		{
			return Err("Invalid channel hooks");
		}
		Ok(())
	}

	/// Removes `plugin` from these hooks; whether anything changed.
	pub fn remove(&mut self, plugin: &str) -> bool {
		let before = (self.send.is_some(), self.annotate.len());
		if self.send.as_ref().is_some_and(|hook| hook.plugin == plugin) {
			self.send = None;
		}
		self.annotate.retain(|id| id != plugin);
		before != (self.send.is_some(), self.annotate.len())
	}
}

/// The saved entry whose notes run in every conversation. It holds annotators only: a send
/// hook rewrites what the user sends, so it stays a choice per channel. No Discord ID is
/// this large.
pub const EVERYWHERE: Id = Id(u64::MAX);

/// Finds `id`'s hooks in a saved list.
pub fn for_channel(hooks: &[ChannelHooks], id: Id) -> Option<&ChannelHooks> {
	hooks.iter().find(|hooks| hooks.channel == id)
}

/// The plugins whose notes run in `channel`: its own, then those on in every conversation,
/// at most [`MAX_ANNOTATORS`].
pub fn annotators(hooks: &[ChannelHooks], channel: Id) -> Vec<String> {
	let mut plugins: Vec<String> = Vec::new();
	for entry in [channel, EVERYWHERE]
		.into_iter()
		.filter_map(|id| for_channel(hooks, id))
	{
		for plugin in &entry.annotate {
			if plugins.len() < MAX_ANNOTATORS && !plugins.contains(plugin) {
				plugins.push(plugin.clone());
			}
		}
	}
	plugins
}

fn content_key(message: &Message) -> u64 {
	use std::hash::{Hash, Hasher};
	let mut hasher = std::collections::hash_map::DefaultHasher::new();
	message.content.hash(&mut hasher);
	hasher.finish()
}

fn annotatable(message: &Message, channel: Id) -> bool {
	message.channel == channel
		&& !message.ephemeral
		&& message.flags & 64 == 0
		&& message.author.id.0 != 0
		&& !message.content.trim().is_empty()
		&& message.content.len() <= MAX_EVENT_CONTENT_BYTES
}

/// Which messages each plugin was given, at which text, so each is annotated once per edit.
#[derive(Default)]
pub struct AnnotateTracker {
	given: HashMap<(String, Id), u64>,
	order: VecDeque<(String, Id)>,
}

/// Messages remembered as given, across plugins.
const MAX_GIVEN: usize = 4096;

impl AnnotateTracker {
	/// Up to 25 of the open channel's latest messages that `plugin` has not seen with their
	/// current text, oldest first.
	pub fn due(&self, state: &State, plugin: &str) -> Option<AnnotateBatch> {
		let channel = state.selected?;
		if !state.can_read_history(channel) {
			return None;
		}
		let mut messages = Vec::new();
		let mut bytes = 0;
		for message in state.timeline.iter().rev().take(ANNOTATE_WINDOW) {
			if !annotatable(message, channel) {
				continue;
			}
			let key = content_key(message);
			let given = self.given.get(&(plugin.to_owned(), message.id));
			if given == Some(&key) {
				continue;
			}
			if bytes + message.content.len() > MAX_ANNOTATE_BYTES {
				break;
			}
			bytes += message.content.len();
			messages.push(AnnotateMessage {
				message_id: message.id.0.to_string(),
				author_id: message.author.id.0.to_string(),
				content: message.content.clone(),
				edited: given.is_some(),
			});
			if messages.len() == MAX_ANNOTATE_MESSAGES {
				break;
			}
		}
		if messages.is_empty() {
			return None;
		}
		messages.reverse();
		Some(AnnotateBatch {
			channel_id: channel.0.to_string(),
			guild_id: state
				.channel(channel)
				.and_then(|channel| channel.guild)
				.map(|guild| guild.0.to_string()),
			messages,
		})
	}

	/// Remembers that `plugin` was given `batch`, whether or not it succeeds, so a failing
	/// plugin is not handed the same messages again.
	pub fn given(&mut self, state: &State, plugin: &str, batch: &AnnotateBatch) {
		for message in &batch.messages {
			let Some(id) = message.message_id.parse().ok().map(Id) else {
				continue;
			};
			let Some(current) = state.timeline.get(id) else {
				continue;
			};
			let key = (plugin.to_owned(), id);
			if self
				.given
				.insert(key.clone(), content_key(current))
				.is_none()
			{
				self.order.push_back(key);
			}
		}
		while self.order.len() > MAX_GIVEN {
			if let Some(old) = self.order.pop_front() {
				self.given.remove(&old);
			}
		}
	}

	pub fn forget(&mut self, plugin: &str) {
		self.given.retain(|(id, _), _| id != plugin);
		self.order.retain(|(id, _)| id != plugin);
	}

	pub fn clear(&mut self) {
		self.given.clear();
		self.order.clear();
	}
}

/// One plugin's additions to a message: a note under it, earlier versions behind its
/// "edited" label, deleted messages the plugin kept, shown as rows under it, and text shown
/// in place of the message, such as its translation.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Note {
	pub plugin: String,
	/// The plugin's name when the note arrived, shown with it.
	pub name: String,
	/// Empty without a note.
	pub text: String,
	/// Oldest first.
	pub history: Vec<Revision>,
	pub deleted: Vec<Archived>,
	/// The conversation `deleted` belongs to, so another one never shows them.
	pub deleted_channel: Option<Id>,
	pub alternate: Option<Alternate>,
}

/// Text shown in place of a message, with a toggle back to the message itself.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Alternate {
	/// What the text is, such as "Translation".
	pub label: String,
	pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Revision {
	pub time_ms: Option<i64>,
	pub text: String,
}

/// A deleted message a plugin kept, shown where it was.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Archived {
	pub message: Id,
	pub author: String,
	pub time_ms: Option<i64>,
	pub text: String,
	pub deleted_ms: Option<i64>,
	/// Oldest first.
	pub history: Vec<Revision>,
}

impl Note {
	fn bytes(&self) -> usize {
		self.plugin.len()
			+ self.name.len()
			+ self.text.len()
			+ self
				.history
				.iter()
				.map(|revision| size_of::<Revision>() + revision.text.len())
				.sum::<usize>()
			+ self
				.deleted
				.iter()
				.map(|row| {
					size_of::<Archived>()
						+ row.author.len() + row.text.len()
						+ row
							.history
							.iter()
							.map(|revision| size_of::<Revision>() + revision.text.len())
							.sum::<usize>()
				})
				.sum::<usize>()
			+ self
				.alternate
				.as_ref()
				.map_or(0, |alternate| alternate.label.len() + alternate.text.len())
	}
	fn is_empty(&self) -> bool {
		self.text.is_empty()
			&& self.history.is_empty()
			&& self.deleted.is_empty()
			&& self.alternate.is_none()
	}
}

/// What plugins added to messages, in memory only and bounded.
#[derive(Default)]
pub struct Annotations {
	notes: HashMap<Id, Vec<Note>>,
	order: VecDeque<Id>,
	bytes: usize,
	/// Every deleted row by its own id and plugin, with the message it was placed under.
	deleted: BTreeMap<(Id, String), Id>,
}

/// Messages with additions kept, and the bytes of all of them.
const MAX_NOTED: usize = 2000;
const MAX_NOTE_BYTES: usize = 2 * 1024 * 1024;

impl Annotations {
	/// Sets `plugin`'s note under `message`; empty text removes it.
	pub fn set(&mut self, plugin: &str, name: &str, message: Id, text: &str) {
		let text = text.trim();
		if text.len() > MAX_ANNOTATION_BYTES {
			return;
		}
		self.update(plugin, name, message, |note| note.text = text.to_owned());
	}
	/// Sets the earlier versions `plugin` knows of `message`; an empty list removes them.
	pub fn set_history(&mut self, plugin: &str, name: &str, message: Id, history: Vec<Revision>) {
		self.update(plugin, name, message, |note| note.history = history);
	}
	/// Sets the deleted messages `plugin` shows under `message`; an empty list removes them.
	///
	/// A plugin given older messages may place a newer deleted one under the newest of
	/// them. Each deleted message stays under the closest earlier message `plugin` placed it
	/// under, so it is shown once, where it was.
	pub fn set_deleted(
		&mut self,
		plugin: &str,
		name: &str,
		message: Id,
		channel: Id,
		mut rows: Vec<Archived>,
	) {
		rows.retain(|row| row.message > message);
		let mut moved = Vec::new();
		rows.retain(|row| {
			let elsewhere = self
				.deleted
				.get(&(row.message, plugin.to_owned()))
				.filter(|anchor| **anchor != message);
			match elsewhere.copied() {
				Some(anchor) if anchor > message => false,
				Some(anchor) => {
					moved.push((anchor, row.message));
					true
				}
				None => true,
			}
		});
		for (anchor, row) in moved {
			self.update(plugin, name, anchor, |note| {
				note.deleted.retain(|kept| kept.message != row);
			});
		}
		self.update(plugin, name, message, |note| {
			note.deleted = rows;
			note.deleted_channel = Some(channel);
		});
	}
	/// Sets the text `plugin` shows in place of `message`; empty text removes it.
	pub fn set_alternate(
		&mut self,
		plugin: &str,
		name: &str,
		message: Id,
		label: &str,
		text: &str,
	) {
		let text = text.trim();
		if text.len() > MAX_ANNOTATION_BYTES {
			return;
		}
		let alternate = (!text.is_empty()).then(|| Alternate {
			label: label.trim().chars().take(64).collect(),
			text: text.to_owned(),
		});
		self.update(plugin, name, message, |note| note.alternate = alternate);
	}
	fn update(&mut self, plugin: &str, name: &str, message: Id, change: impl FnOnce(&mut Note)) {
		let notes = self.notes.entry(message).or_default();
		let mut note = match notes.iter().position(|note| note.plugin == plugin) {
			Some(index) => {
				let note = notes.remove(index);
				self.bytes -= note.bytes();
				for row in &note.deleted {
					self.deleted.remove(&(row.message, note.plugin.clone()));
				}
				note
			}
			None => Note {
				plugin: plugin.to_owned(),
				..Default::default()
			},
		};
		note.name = name.chars().take(64).collect();
		change(&mut note);
		if !note.is_empty() {
			self.bytes += note.bytes();
			for row in &note.deleted {
				self.deleted
					.insert((row.message, note.plugin.clone()), message);
			}
			notes.push(note);
			notes.sort_by(|a, b| a.plugin.cmp(&b.plugin));
		}
		if notes.is_empty() {
			self.notes.remove(&message);
			self.order.retain(|id| *id != message);
		} else if !self.order.contains(&message) {
			self.order.push_back(message);
		}
		while self.order.len() > MAX_NOTED || self.bytes > MAX_NOTE_BYTES {
			let Some(old) = self.order.pop_front() else {
				break;
			};
			for note in self.notes.remove(&old).unwrap_or_default() {
				self.bytes -= note.bytes();
				for row in &note.deleted {
					self.deleted.remove(&(row.message, note.plugin.clone()));
				}
			}
		}
	}

	/// The note whose alternate replaces `message`'s text: the first plugin's, in plugin order.
	pub fn alternate(&self, message: Id) -> Option<&Note> {
		self.get(message)
			.iter()
			.find(|note| note.alternate.is_some())
	}

	/// The notes under `message`, in plugin order.
	pub fn get(&self, message: Id) -> &[Note] {
		self.notes.get(&message).map_or(&[], Vec::as_slice)
	}

	/// The deleted rows plugins kept in `channel` that fall after message `after` and before
	/// `before`, by their own ids, oldest first, with the note that holds each. A timeline
	/// draws them after the message shown before them, whichever message a plugin placed them
	/// under: a plugin is not given every message, such as one with only pictures.
	pub fn deleted_between(
		&self,
		channel: Id,
		after: Id,
		before: Option<Id>,
	) -> impl Iterator<Item = (&Note, &Archived)> {
		let end = before.unwrap_or(Id(u64::MAX));
		let start = Id(after.0.saturating_add(1)).min(end);
		self.deleted
			.range((start, String::new())..(end, String::new()))
			.filter_map(move |((row, plugin), anchor)| {
				let note = self
					.get(*anchor)
					.iter()
					.find(|note| note.plugin == *plugin && note.deleted_channel == Some(channel))?;
				Some((note, note.deleted.iter().find(|kept| kept.message == *row)?))
			})
	}

	/// Changes whenever [`Self::deleted_between`] would show something else; 0 without rows.
	pub fn deleted_fingerprint(&self, channel: Id, after: Id, before: Option<Id>) -> u64 {
		use std::hash::{Hash, Hasher};
		let mut rows = self.deleted_between(channel, after, before).peekable();
		if rows.peek().is_none() {
			return 0;
		}
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		for (note, row) in rows {
			(&note.name, row, self.get(row.message)).hash(&mut hasher);
		}
		hasher.finish()
	}

	/// Changes whenever the notes under `message` change; 0 without notes. Row layouts
	/// cached by message include it.
	pub fn fingerprint(&self, message: Id) -> u64 {
		use std::hash::{Hash, Hasher};
		let Some(notes) = self.notes.get(&message) else {
			return 0;
		};
		let mut hasher = std::collections::hash_map::DefaultHasher::new();
		notes.hash(&mut hasher);
		hasher.finish()
	}

	pub fn remove_plugin(&mut self, plugin: &str) {
		let messages: Vec<Id> = self
			.notes
			.iter()
			.filter(|(_, notes)| notes.iter().any(|note| note.plugin == plugin))
			.map(|(id, _)| *id)
			.collect();
		for message in messages {
			self.update(plugin, "", message, |note| *note = Note::default());
		}
	}

	pub fn clear(&mut self) {
		*self = Self::default();
	}

	pub fn is_empty(&self) -> bool {
		self.notes.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn latest_messages_are_due_once_per_text_and_edits_come_back() {
		let mut state = test_support::demo_state();
		let channel = state.selected.unwrap();
		for id in 0..30 {
			let mut message = test_support::message(5000 + id, channel);
			message.content = format!("message {id}");
			if id == 3 {
				message.ephemeral = true;
			}
			state.apply(client_core::Envelope {
				generation: state.generation,
				event: client_core::Event::Message(message),
			});
		}
		let mut tracker = AnnotateTracker::default();
		let batch = tracker.due(&state, "translator").unwrap();
		assert_eq!(batch.messages.len(), MAX_ANNOTATE_MESSAGES);
		// Oldest first, ending with the newest message.
		let ids: Vec<u64> = batch
			.messages
			.iter()
			.map(|m| m.message_id.parse().unwrap())
			.collect();
		assert!(ids.is_sorted() && *ids.last().unwrap() == 5029);
		tracker.given(&state, "translator", &batch);
		// The rest of the window follows in batches, never the ephemeral message, then nothing.
		for _ in 0..ANNOTATE_WINDOW / MAX_ANNOTATE_MESSAGES {
			let Some(rest) = tracker.due(&state, "translator") else {
				break;
			};
			assert!(rest.messages.iter().all(|m| m.message_id != "5003"));
			tracker.given(&state, "translator", &rest);
		}
		assert!(tracker.due(&state, "translator").is_none());
		// Another plugin starts from scratch; an edit is due again and flagged.
		assert!(tracker.due(&state, "other").is_some());
		let mut edited = test_support::message(5029, channel);
		edited.content = "edited text".into();
		state.apply(client_core::Envelope {
			generation: state.generation,
			event: client_core::Event::Message(edited),
		});
		let again = tracker.due(&state, "translator").unwrap();
		assert_eq!(again.messages.len(), 1);
		assert!(again.messages[0].edited && again.messages[0].content == "edited text");
	}

	/// Notes on in every conversation join a channel's own, once each and within the
	/// per-channel cap; the everywhere entry never carries a send hook.
	#[test]
	fn everywhere_notes_join_each_channel_within_the_cap() {
		let entry = |channel, plugins: &[&str]| ChannelHooks {
			channel,
			send: None,
			annotate: plugins.iter().map(|p| p.to_string()).collect(),
		};
		let saved = [
			entry(Id(7), &["a", "b", "c"]),
			entry(EVERYWHERE, &["b", "d", "e"]),
		];
		assert_eq!(annotators(&saved, Id(7)), ["a", "b", "c", "d"]);
		assert_eq!(annotators(&saved, Id(8)), ["b", "d", "e"]);
		let mut with_send = entry(EVERYWHERE, &[]);
		with_send.send = Some(SendHook {
			plugin: "a".into(),
			mode: SendMode::Auto,
			fallback: Fallback::SendOriginal,
		});
		assert!(with_send.validate().is_err());
		assert!(entry(EVERYWHERE, &["a"]).validate().is_ok());
	}

	#[test]
	fn notes_are_bounded_and_removed_per_plugin() {
		let mut notes = Annotations::default();
		notes.set("b", "B", Id(1), "second");
		notes.set("a", "A", Id(1), "first");
		assert_eq!(notes.get(Id(1))[0].text, "first");
		let before = notes.fingerprint(Id(1));
		notes.set("a", "A", Id(1), "");
		assert_eq!(notes.get(Id(1)).len(), 1);
		assert_ne!(
			notes.fingerprint(Id(1)),
			before,
			"row layouts see the change"
		);
		// A history or deleted rows keep a plugin's entry without a note, and clearing all
		// three removes it.
		let revision = Revision {
			time_ms: None,
			text: "earlier".into(),
		};
		notes.set_history("a", "A", Id(1), vec![revision.clone()]);
		notes.set_deleted(
			"a",
			"A",
			Id(1),
			Id(7),
			vec![Archived {
				message: Id(2),
				author: "Ada".into(),
				time_ms: None,
				text: "gone".into(),
				deleted_ms: Some(5),
				history: vec![revision.clone()],
			}],
		);
		notes.set("a", "A", Id(1), "");
		let kept = &notes.get(Id(1))[0];
		assert_eq!((kept.history.len(), kept.deleted.len()), (1, 1));
		notes.set_history("a", "A", Id(1), Vec::new());
		notes.set_deleted("a", "A", Id(1), Id(7), Vec::new());
		assert_eq!(notes.get(Id(1)).len(), 1, "only plugin b's note is left");
		// An alternate alone keeps an entry, and the first plugin's is shown.
		notes.set_alternate("c", "C", Id(1), "Translation", "later plugin");
		notes.set_alternate("a", "A", Id(1), "Translation", "hello");
		assert_eq!(notes.alternate(Id(1)).unwrap().plugin, "a");
		notes.set_alternate("a", "A", Id(1), "Translation", "");
		notes.set_alternate("c", "C", Id(1), "", " ");
		assert!(notes.alternate(Id(1)).is_none());
		assert_eq!(notes.get(Id(1)).len(), 1, "only plugin b's note is left");
		notes.set_history("a", "A", Id(1), vec![revision]);
		for id in 0..MAX_NOTED as u64 + 10 {
			notes.set("a", "A", Id(100 + id), "x");
		}
		assert_eq!(notes.order.len(), MAX_NOTED);
		assert!(notes.get(Id(1)).is_empty(), "oldest message evicted");
		notes.remove_plugin("a");
		assert!(notes.is_empty() && notes.bytes == 0);
	}

	#[test]
	fn deleted_rows_show_once_by_their_own_time() {
		let mut notes = Annotations::default();
		let gone = |id| Archived {
			message: Id(id),
			author: "Ada".into(),
			time_ms: None,
			text: "gone".into(),
			deleted_ms: None,
			history: Vec::new(),
		};
		let shown = |notes: &Annotations, anchor| {
			notes
				.get(Id(anchor))
				.iter()
				.flat_map(|note| note.deleted.iter().map(|row| row.message.0))
				.collect::<Vec<_>>()
		};
		// The newest batch places 90 under 80; an older batch, ending at 40, places it
		// there too, with 30 that belongs under it.
		notes.set_deleted("a", "A", Id(80), Id(7), vec![gone(90)]);
		notes.set_deleted("a", "A", Id(40), Id(7), vec![gone(30), gone(45), gone(90)]);
		assert_eq!(shown(&notes, 80), [90]);
		assert_eq!(shown(&notes, 40), [45], "not a row before its own message");
		// A closer message moves it; another plugin keeps its own placement.
		notes.set_deleted("a", "A", Id(85), Id(7), vec![gone(90)]);
		notes.set_deleted("b", "B", Id(40), Id(7), vec![gone(90)]);
		assert_eq!((shown(&notes, 80), shown(&notes, 85)), (vec![], vec![90]));
		assert_eq!(shown(&notes, 40), [45, 90]);
		// Drawn by their own ids after the message shown before them, such as a picture the
		// plugin was never given, whichever message they were placed under.
		let between = |notes: &Annotations, after: u64, before: Option<u64>| {
			notes
				.deleted_between(Id(7), Id(after), before.map(Id))
				.map(|(note, row)| (note.plugin.clone(), row.message.0))
				.collect::<Vec<_>>()
		};
		let row = |plugin: &str, id| (plugin.to_owned(), id);
		assert_eq!(
			between(&notes, 41, Some(100)),
			[row("a", 45), row("a", 90), row("b", 90)]
		);
		assert_eq!(between(&notes, 45, Some(90)), [], "not the rows themselves");
		assert_eq!(between(&notes, 88, None), [row("a", 90), row("b", 90)]);
		// Another conversation's timeline shows none of them, as message ids say nothing
		// about where a message was.
		assert_eq!(notes.deleted_between(Id(8), Id(0), None).count(), 0);
		assert_eq!(notes.deleted_fingerprint(Id(8), Id(0), None), 0);
		// Its earlier versions, kept under its own id, change the rows it is drawn in.
		let before = notes.deleted_fingerprint(Id(7), Id(88), None);
		notes.set_history(
			"a",
			"A",
			Id(90),
			vec![Revision {
				time_ms: None,
				text: "first".into(),
			}],
		);
		assert_ne!(notes.deleted_fingerprint(Id(7), Id(88), None), before);
		// Removing a plugin, or evicting its notes, drops its rows from the order too.
		notes.remove_plugin("b");
		assert_eq!(
			between(&notes, 0, None),
			[("a".to_owned(), 45), ("a".to_owned(), 90)]
		);
		for id in 0..MAX_NOTED as u64 {
			notes.set("c", "C", Id(1000 + id), "x");
		}
		assert_eq!(between(&notes, 0, None), []);
	}
}
