//! Message hooks the user switched on per channel, the notes plugins add under messages, and
//! which messages of the open channel are due for a plugin's `annotate` action.
use client_core::State;
use extensions::{
	AnnotateBatch, AnnotateMessage, MAX_EVENT_CONTENT_BYTES,
	message_hooks::{MAX_ANNOTATE_BYTES, MAX_ANNOTATE_MESSAGES, MAX_ANNOTATION_BYTES},
};
use model::{Id, Message};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};

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

/// Finds `id`'s hooks in a saved list.
pub fn for_channel(hooks: &[ChannelHooks], id: Id) -> Option<&ChannelHooks> {
	hooks.iter().find(|hooks| hooks.channel == id)
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
/// "edited" label, and deleted messages the plugin kept, shown as rows under it.
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
				.map(|row| size_of::<Archived>() + row.author.len() + row.text.len())
				.sum::<usize>()
	}
	fn is_empty(&self) -> bool {
		self.text.is_empty() && self.history.is_empty() && self.deleted.is_empty()
	}
}

/// What plugins added to messages, in memory only and bounded.
#[derive(Default)]
pub struct Annotations {
	notes: HashMap<Id, Vec<Note>>,
	order: VecDeque<Id>,
	bytes: usize,
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
	pub fn set_deleted(&mut self, plugin: &str, name: &str, message: Id, rows: Vec<Archived>) {
		self.update(plugin, name, message, |note| note.deleted = rows);
	}
	fn update(&mut self, plugin: &str, name: &str, message: Id, change: impl FnOnce(&mut Note)) {
		let notes = self.notes.entry(message).or_default();
		let mut note = match notes.iter().position(|note| note.plugin == plugin) {
			Some(index) => {
				let note = notes.remove(index);
				self.bytes -= note.bytes();
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
			}
		}
	}

	/// The notes under `message`, in plugin order.
	pub fn get(&self, message: Id) -> &[Note] {
		self.notes.get(&message).map_or(&[], Vec::as_slice)
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
			vec![Archived {
				message: Id(2),
				author: "Ada".into(),
				time_ms: None,
				text: "gone".into(),
			}],
		);
		notes.set("a", "A", Id(1), "");
		let kept = &notes.get(Id(1))[0];
		assert_eq!((kept.history.len(), kept.deleted.len()), (1, 1));
		notes.set_history("a", "A", Id(1), Vec::new());
		notes.set_deleted("a", "A", Id(1), Vec::new());
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
}
