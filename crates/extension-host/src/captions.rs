//! Caption lines plugins show above the call controls, in memory only. Each plugin's lines
//! replace its previous ones and fade out [`LINGER`] after its last update.
use extensions::Caption;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long caption lines stay after the plugin's last update.
pub const LINGER: Duration = Duration::from_secs(10);
/// Plugins whose captions are shown at once; more wait until one goes quiet.
const MAX_PLUGINS: usize = 2;

#[derive(Default)]
pub struct Captions {
	lines: BTreeMap<String, (String, Vec<Caption>, Instant)>,
}

impl Captions {
	/// Replaces `plugin`'s lines; an empty list clears them.
	pub fn set(&mut self, plugin: &str, name: &str, captions: Vec<Caption>, now: Instant) {
		if captions.is_empty() {
			self.lines.remove(plugin);
			return;
		}
		self.expire(now);
		if !self.lines.contains_key(plugin) && self.lines.len() >= MAX_PLUGINS {
			return;
		}
		let name = name.chars().take(64).collect();
		self.lines.insert(plugin.to_owned(), (name, captions, now));
	}
	/// Drops lines that went quiet; returns when the next ones do.
	pub fn expire(&mut self, now: Instant) -> Option<Duration> {
		self.lines
			.retain(|_, (_, _, at)| now.saturating_duration_since(*at) < LINGER);
		self.lines
			.values()
			.map(|(_, _, at)| LINGER.saturating_sub(now.saturating_duration_since(*at)))
			.min()
	}
	/// The lines to show, as (plugin name, line), in plugin order.
	pub fn lines(&self) -> impl Iterator<Item = (&str, &Caption)> {
		self.lines
			.values()
			.flat_map(|(name, lines, _)| lines.iter().map(move |line| (name.as_str(), line)))
	}
	pub fn remove_plugin(&mut self, plugin: &str) {
		self.lines.remove(plugin);
	}
	pub fn clear(&mut self) {
		self.lines.clear();
	}
	pub fn is_empty(&self) -> bool {
		self.lines.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn line(text: &str) -> Caption {
		Caption {
			speaker_id: None,
			text: text.into(),
			done: false,
		}
	}

	/// A plugin's lines replace its earlier ones and fade after [`LINGER`]; an empty list
	/// clears them, and a third plugin waits while two are showing.
	#[test]
	fn caption_lines_replace_fade_and_are_bounded_per_plugin() {
		let now = Instant::now();
		let mut captions = Captions::default();
		captions.set("a", "A", vec![line("hel")], now);
		captions.set("a", "A", vec![line("hello")], now);
		captions.set("b", "B", vec![line("hola")], now + Duration::from_secs(5));
		captions.set("c", "C", vec![line("hallo")], now);
		let shown: Vec<_> = captions
			.lines()
			.map(|(name, line)| (name, line.text.as_str()))
			.collect();
		assert_eq!(shown, [("A", "hello"), ("B", "hola")]);
		assert_eq!(captions.expire(now + LINGER), Some(Duration::from_secs(5)));
		assert_eq!(captions.lines().count(), 1, "plugin a went quiet");
		captions.set("b", "B", Vec::new(), now + LINGER);
		assert!(captions.is_empty());
	}
}
