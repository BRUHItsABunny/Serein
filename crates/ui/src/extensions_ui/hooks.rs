//! Message hooks in the composer: the per-channel switches, running a plugin's `send` action
//! when the user sends, and the notes `annotate` results add under messages.
use super::{ExtensionContext, ExtensionRequest, ExtensionUi};
use crate::design;
use client_core::State;
use extension_host::hooks::{Annotations, ChannelHooks, Fallback, SendHook, SendMode};
use extensions::{Annotation, Invocation, SendTarget, Surface};
use model::Id;

/// A draft handed to a send hook, waiting for its result.
pub(crate) struct Pending {
	channel: Id,
	generation: u64,
	plugin: String,
	name: String,
	mode: SendMode,
	fallback: Fallback,
	original: String,
}

/// What became of a draft the send hook handled, applied by the composer.
pub(crate) enum Outcome {
	/// Put `text` in the draft; with `send`, send it at once.
	Replace { text: String, send: bool },
	/// Send the draft as it is.
	Send,
	/// Keep the draft and say why.
	Keep(String),
}

/// A rewrite shown in the draft for the user to review.
struct Review {
	channel: Id,
	name: String,
	original: String,
}

#[derive(Default)]
pub(crate) struct SendState {
	pending: Option<Pending>,
	outcome: Option<(Id, u64, String, Outcome)>,
	review: Option<Review>,
	/// The next send in this channel skips the hook.
	bypass: Option<Id>,
}

pub(crate) enum NoteUpdate {
	/// A plugin's note under a message, with the plugin's name; empty text removes it.
	Set {
		plugin: String,
		name: String,
		message: Id,
		text: String,
	},
	/// Every note of one plugin.
	Forget(String),
	/// Every note, such as when the account changes.
	Clear,
}

impl ExtensionUi {
	/// The plugins with a running action on `surface`, as (ID, name, action).
	fn hook_plugins(&self, surface: Surface) -> impl Iterator<Item = (&str, &str, &str)> {
		self.entries
			.iter()
			.filter(|entry| entry.enabled)
			.filter_map(move |entry| {
				let action = entry
					.manifest
					.actions
					.iter()
					.find(|action| action.surface == surface)?;
				Some((
					entry.manifest.id.as_str(),
					entry.manifest.name.as_str(),
					action.id.as_str(),
				))
			})
	}

	fn hooks_in(&self, channel: Id) -> ChannelHooks {
		extension_host::hooks::for_channel(&self.channel_hooks, channel)
			.cloned()
			.unwrap_or(ChannelHooks {
				channel,
				send: None,
				annotate: Vec::new(),
			})
	}

	/// The send hook switched on in `channel` whose plugin runs, with its name and action.
	fn send_hook(&self, channel: Id) -> Option<(SendHook, String, String)> {
		let hook = extension_host::hooks::for_channel(&self.channel_hooks, channel)?
			.send
			.clone()?;
		let (_, name, action) = self
			.hook_plugins(Surface::Send)
			.find(|(id, _, _)| *id == hook.plugin)?;
		let (name, action) = (name.to_owned(), action.to_owned());
		Some((hook, name, action))
	}

	pub(crate) fn has_channel_hooks(&self) -> bool {
		self.hook_plugins(Surface::Send)
			.chain(self.hook_plugins(Surface::Annotate))
			.next()
			.is_some()
	}

	/// Hands the draft to the channel's send hook instead of sending it; false when the
	/// message should be sent now.
	pub(crate) fn intercept_send(&mut self, state: &State, channel: Id) -> bool {
		if self
			.send
			.bypass
			.take_if(|bypass| *bypass == channel)
			.is_some()
		{
			self.send.review = None;
			return false;
		}
		if self
			.send
			.pending
			.as_ref()
			.is_some_and(|pending| pending.channel == channel)
		{
			return true;
		}
		let draft = state.drafts.get(&channel).map_or("", String::as_str);
		// The user reviewed this rewrite; send it, edited or not.
		if self
			.send
			.review
			.take_if(|review| review.channel == channel)
			.is_some()
		{
			return false;
		}
		let Some((hook, name, action)) = self.send_hook(channel) else {
			return false;
		};
		// Attachments without text have nothing to rewrite.
		if draft.trim().is_empty() {
			return false;
		}
		let target = SendTarget {
			channel_id: channel.0.to_string(),
			guild_id: state
				.channel(channel)
				.and_then(|channel| channel.guild)
				.map(|guild| guild.0.to_string()),
		};
		self.requests.push(ExtensionRequest::Invoke {
			id: hook.plugin.clone(),
			invocation: Invocation {
				action,
				composer: Some(draft.to_owned()),
				send_target: Some(Box::new(target)),
				..Default::default()
			},
			context: ExtensionContext::capture(state, true),
		});
		self.send.pending = Some(Pending {
			channel,
			generation: state.generation,
			plugin: hook.plugin,
			name,
			mode: hook.mode,
			fallback: hook.fallback,
			original: draft.to_owned(),
		});
		true
	}

	/// Takes a finished hook's outcome for `channel`, to apply to its draft.
	pub(crate) fn take_send_outcome(&mut self, state: &State, channel: Id) -> Option<Outcome> {
		let (target, generation, original, outcome) = self.send.outcome.take()?;
		if generation != state.generation {
			return None;
		}
		if target != channel {
			self.send.outcome = Some((target, generation, original, outcome));
			return None;
		}
		if state.drafts.get(&channel).map_or("", String::as_str) != original {
			self.send.review = None;
			return Some(Outcome::Keep(crate::i18n::translate(
				"extensions-ui-hook-draft-changed",
			)));
		}
		if let Outcome::Replace { send: true, .. } | Outcome::Send = &outcome {
			self.send.bypass = Some(channel);
		}
		Some(outcome)
	}

	/// A send hook's result arrived.
	pub(super) fn send_result(
		&mut self,
		id: &str,
		invocation: &Invocation,
		replacement: Option<String>,
	) {
		let Some(pending) = self.send.pending.take_if(|pending| {
			pending.plugin == id
				&& invocation.composer.as_deref() == Some(pending.original.as_str())
				&& invocation
					.send_target
					.as_ref()
					.is_some_and(|target| target.channel_id == pending.channel.0.to_string())
		}) else {
			return;
		};
		let outcome = match replacement.filter(|text| *text != pending.original) {
			None => Outcome::Send,
			Some(text) if text.trim().is_empty() || text.len() > client_core::MAX_DRAFT_BYTES => {
				Outcome::Keep(crate::i18n::translate_args(
					"extensions-ui-hook-unusable",
					&[("plugin", &pending.name)],
				))
			}
			Some(text) => {
				let send = pending.mode == SendMode::Auto;
				if !send {
					self.send.review = Some(Review {
						channel: pending.channel,
						name: pending.name.clone(),
						original: pending.original.clone(),
					});
				}
				Outcome::Replace { text, send }
			}
		};
		self.send.outcome = Some((
			pending.channel,
			pending.generation,
			pending.original,
			outcome,
		));
	}

	/// A send hook failed; its fallback decides whether the draft is sent unchanged.
	pub fn send_failed(&mut self, id: &str, invocation: &Invocation) -> bool {
		if invocation.send_target.is_none() {
			return false;
		}
		let Some(pending) = self.send.pending.take_if(|pending| pending.plugin == id) else {
			return true;
		};
		let outcome = match pending.fallback {
			Fallback::SendOriginal => Outcome::Send,
			Fallback::KeepDraft => Outcome::Keep(crate::i18n::translate_args(
				"extensions-ui-hook-failed",
				&[("plugin", &pending.name)],
			)),
		};
		self.send.outcome = Some((
			pending.channel,
			pending.generation,
			pending.original,
			outcome,
		));
		true
	}

	/// The line above the composer while a hook handles or rewrote the draft, or a reminder
	/// that one is switched on here.
	pub(crate) fn send_hook_bar(&mut self, ui: &mut egui::Ui, state: &mut State, channel: Id) {
		// Clearing the reviewed draft ends the review; new text goes through the hook again.
		if state
			.drafts
			.get(&channel)
			.is_none_or(|draft| draft.trim().is_empty())
		{
			self.send.review.take_if(|review| review.channel == channel);
		}
		let colors = design::palette(ui);
		let pending = self
			.send
			.pending
			.as_ref()
			.filter(|pending| pending.channel == channel);
		let review = self
			.send
			.review
			.as_ref()
			.filter(|review| review.channel == channel);
		let (text, button) = if let Some(pending) = pending {
			(
				crate::i18n::translate_args(
					"extensions-ui-hook-pending",
					&[("plugin", &pending.name)],
				),
				Some("extensions-ui-hook-cancel"),
			)
		} else if let Some(review) = review {
			(
				crate::i18n::translate_args(
					"extensions-ui-hook-review",
					&[("plugin", &review.name)],
				),
				Some("extensions-ui-hook-use-original"),
			)
		} else if let Some((hook, name, _)) = self.send_hook(channel) {
			(
				crate::i18n::translate_args(
					if hook.mode == SendMode::Auto {
						"extensions-ui-hook-active-auto"
					} else {
						"extensions-ui-hook-active-preview"
					},
					&[("plugin", &name)],
				),
				None,
			)
		} else {
			return;
		};
		let mut clicked = false;
		ui.horizontal_wrapped(|ui| {
			ui.spacing_mut().item_spacing.x = 6.0;
			ui.label(egui::RichText::new(text).small().color(colors.muted));
			if let Some(button) = button {
				clicked =
					super::quiet_action(ui, button, colors.muted, colors.text_strong).clicked();
			}
		});
		if !clicked {
			return;
		}
		if self
			.send
			.pending
			.take_if(|p| p.channel == channel)
			.is_none()
			&& let Some(review) = self.send.review.take_if(|r| r.channel == channel)
		{
			// Back to the original; the next send skips the hook, as the user chose it.
			state.drafts.insert(channel, review.original);
			self.send.bypass = Some(channel);
		}
	}

	/// The switches for this channel's hooks, in the composer's tools menu.
	pub(super) fn channel_hooks_menu(&mut self, ui: &mut egui::Ui, state: &State) {
		let Some(channel) = state.selected else {
			return;
		};
		let senders: Vec<(String, String)> = self
			.hook_plugins(Surface::Send)
			.map(|(id, name, _)| (id.to_owned(), name.to_owned()))
			.collect();
		let annotators: Vec<(String, String)> = self
			.hook_plugins(Surface::Annotate)
			.map(|(id, name, _)| (id.to_owned(), name.to_owned()))
			.collect();
		if senders.is_empty() && annotators.is_empty() {
			return;
		}
		ui.separator();
		ui.label(
			egui::RichText::new(crate::i18n::translate("extensions-ui-hook-menu-heading"))
				.small()
				.strong(),
		);
		let before = self.hooks_in(channel);
		let mut hooks = before.clone();
		for (id, name) in &senders {
			let mut on = hooks.send.as_ref().is_some_and(|hook| hook.plugin == *id);
			if ui
				.checkbox(
					&mut on,
					crate::i18n::translate_args("extensions-ui-hook-send", &[("plugin", name)]),
				)
				.changed()
			{
				// One send hook per channel: switching one on replaces another.
				hooks.send = on.then(|| SendHook {
					plugin: id.clone(),
					mode: SendMode::Preview,
					fallback: Fallback::KeepDraft,
				});
			}
			if let Some(hook) = hooks.send.as_mut().filter(|hook| hook.plugin == *id) {
				ui.indent(("send-hook-options", id), |ui| {
					for (mode, key) in [
						(SendMode::Preview, "extensions-ui-hook-mode-preview"),
						(SendMode::Auto, "extensions-ui-hook-mode-auto"),
					] {
						ui.radio_value(&mut hook.mode, mode, crate::i18n::translate(key));
					}
					ui.label(
						egui::RichText::new(crate::i18n::translate("extensions-ui-hook-fallback"))
							.small(),
					);
					for (fallback, key) in [
						(Fallback::KeepDraft, "extensions-ui-hook-fallback-keep"),
						(Fallback::SendOriginal, "extensions-ui-hook-fallback-send"),
					] {
						ui.radio_value(&mut hook.fallback, fallback, crate::i18n::translate(key));
					}
				});
			}
		}
		for (id, name) in &annotators {
			let mut on = hooks.annotate.contains(id);
			if ui
				.add_enabled(
					on || hooks.annotate.len() < extension_host::hooks::MAX_ANNOTATORS,
					egui::Checkbox::new(
						&mut on,
						crate::i18n::translate_args(
							"extensions-ui-hook-annotate",
							&[("plugin", name)],
						),
					),
				)
				.changed()
			{
				hooks.annotate.retain(|plugin| plugin != id);
				if on {
					hooks.annotate.push(id.clone());
				} else {
					self.notes.push(NoteUpdate::Forget(id.clone()));
				}
			}
		}
		if hooks != before {
			// Shown at once; the saved list from the host replaces it.
			self.channel_hooks.retain(|saved| saved.channel != channel);
			if !hooks.is_empty() {
				self.channel_hooks.push(hooks.clone());
			}
			if hooks.send != before.send {
				self.send.pending = None;
				self.send.review = None;
			}
			self.requests
				.push(ExtensionRequest::SetChannelHooks { hooks });
		}
	}

	/// Notes a plugin returned, shown under their messages.
	pub fn receive_annotations(&mut self, id: &str, annotations: &[Annotation]) {
		let Some(name) = self
			.entries
			.iter()
			.find(|entry| entry.manifest.id == id)
			.map(|entry| entry.manifest.name.clone())
		else {
			return;
		};
		// Until the timeline takes them, keep the newest few batches.
		const MAX_WAITING: usize = 8 * extensions::message_hooks::MAX_ANNOTATE_MESSAGES;
		if self.notes.len() + annotations.len() > MAX_WAITING {
			let excess = self.notes.len() + annotations.len() - MAX_WAITING;
			let mut dropped = 0;
			self.notes.retain(|update| {
				let drop = dropped < excess && matches!(update, NoteUpdate::Set { .. });
				dropped += usize::from(drop);
				!drop
			});
		}
		for annotation in annotations {
			if let Ok(message) = annotation.message_id.parse() {
				self.notes.push(NoteUpdate::Set {
					plugin: id.to_owned(),
					name: name.clone(),
					message: Id(message),
					text: annotation.text.clone(),
				});
			}
		}
	}

	/// Removes a plugin's notes, such as when it stops running, or with `None` every note
	/// and pending send hook, such as when the account changes.
	pub fn forget_annotations(&mut self, id: Option<&str>) {
		match id {
			Some(id) => {
				self.notes.push(NoteUpdate::Forget(id.to_owned()));
				if self
					.send
					.pending
					.as_ref()
					.is_some_and(|pending| pending.plugin == id)
				{
					self.send.pending = None;
				}
			}
			None => {
				self.notes.clear();
				self.notes.push(NoteUpdate::Clear);
				self.send = SendState::default();
			}
		}
	}

	/// Moves arrived notes into the timeline's notes.
	pub(crate) fn drain_notes(&mut self, into: &mut Annotations) {
		for update in self.notes.drain(..) {
			match update {
				NoteUpdate::Set {
					plugin,
					name,
					message,
					text,
				} => into.set(&plugin, &name, message, &text),
				NoteUpdate::Forget(plugin) => into.remove_plugin(&plugin),
				NoteUpdate::Clear => into.clear(),
			}
		}
	}
}
