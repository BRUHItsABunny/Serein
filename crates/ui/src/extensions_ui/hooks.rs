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
	/// Sent after the reviewed draft.
	follow_up: Option<String>,
}

/// A send hook's second message, waiting for the first to be confirmed.
struct FollowUp {
	channel: Id,
	generation: u64,
	nonce: String,
	name: String,
	text: String,
}

/// Follow-ups waiting at once; more are dropped with a notice.
const MAX_FOLLOW_UPS: usize = 8;

#[derive(Default)]
pub(crate) struct SendState {
	pending: Option<Pending>,
	outcome: Option<(Id, u64, String, Outcome)>,
	review: Option<Review>,
	/// The next send in this channel skips the hook.
	bypass: Option<Id>,
	/// The follow-up for the next send in this channel, with the plugin's name.
	armed: Option<(Id, String, String)>,
	follow_ups: Vec<FollowUp>,
	/// The follow-up of the outcome waiting to be applied.
	outcome_follow_up: Option<(String, String)>,
}

pub(crate) enum NoteUpdate {
	/// A plugin's note under a message, with the plugin's name; empty text removes it.
	Set {
		plugin: String,
		name: String,
		message: Id,
		text: String,
	},
	/// A plugin's earlier versions of a message; an empty list removes them.
	History {
		plugin: String,
		name: String,
		message: Id,
		history: Vec<extension_host::hooks::Revision>,
	},
	/// Deleted messages a plugin shows under a message; an empty list removes them.
	Deleted {
		plugin: String,
		name: String,
		message: Id,
		rows: Vec<extension_host::hooks::Archived>,
	},
	/// Text a plugin shows in place of a message; empty text removes it.
	Alternate {
		plugin: String,
		name: String,
		message: Id,
		label: String,
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
		if let Some(review) = self.send.review.take_if(|review| review.channel == channel) {
			self.send.armed = review.follow_up.map(|text| (channel, review.name, text));
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
		let follow_up = self.send.outcome_follow_up.take();
		if state.drafts.get(&channel).map_or("", String::as_str) != original {
			self.send.review = None;
			return Some(Outcome::Keep(crate::i18n::translate(
				"extensions-ui-hook-draft-changed",
			)));
		}
		if let Outcome::Replace { send: true, .. } | Outcome::Send = &outcome {
			self.send.bypass = Some(channel);
			self.send.armed = follow_up.map(|(name, text)| (channel, name, text));
		}
		Some(outcome)
	}

	/// A send hook's result arrived.
	pub(super) fn send_result(
		&mut self,
		id: &str,
		invocation: &Invocation,
		replacement: Option<String>,
		follow_up: Option<String>,
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
		let follow_up = follow_up
			.filter(|text| !text.trim().is_empty() && text.len() <= client_core::MAX_DRAFT_BYTES);
		self.send.outcome_follow_up = None;
		let outcome = match replacement.filter(|text| *text != pending.original) {
			None if follow_up.is_none() => Outcome::Send,
			Some(text) if text.trim().is_empty() || text.len() > client_core::MAX_DRAFT_BYTES => {
				Outcome::Keep(crate::i18n::translate_args(
					"extensions-ui-hook-unusable",
					&[("plugin", &pending.name)],
				))
			}
			// A follow-up alone still waits for review: the draft goes first, unchanged.
			rewrite => {
				let text = rewrite.unwrap_or_else(|| pending.original.clone());
				let send = pending.mode == SendMode::Auto;
				if send {
					self.send.outcome_follow_up =
						follow_up.map(|follow_up| (pending.name.clone(), follow_up));
				} else {
					self.send.review = Some(Review {
						channel: pending.channel,
						name: pending.name.clone(),
						original: pending.original.clone(),
						follow_up,
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

	/// The composer queued `command` in `channel`: the armed follow-up, if any, now waits
	/// for it to be confirmed. Any other send drops it.
	pub(crate) fn sent(
		&mut self,
		state: &State,
		channel: Id,
		command: Option<&client_core::Command>,
	) {
		let Some((armed, name, text)) = self.send.armed.take() else {
			return;
		};
		let Some(client_core::Command::Send { nonce, .. }) = command else {
			return;
		};
		if armed != channel {
			return;
		}
		if self.send.follow_ups.len() >= MAX_FOLLOW_UPS {
			self.send.follow_ups.remove(0);
		}
		self.send.follow_ups.push(FollowUp {
			channel,
			generation: state.generation,
			nonce: nonce.clone(),
			name,
			text,
		});
	}

	/// Sends each follow-up whose message was confirmed, and drops those whose message
	/// failed; returns the commands and a notice per dropped follow-up.
	pub(crate) fn send_follow_ups(
		&mut self,
		state: &mut State,
	) -> (Vec<client_core::Command>, Vec<String>) {
		let mut commands = Vec::new();
		let mut notices = Vec::new();
		if self.send.follow_ups.is_empty() {
			return (commands, notices);
		}
		for follow_up in std::mem::take(&mut self.send.follow_ups) {
			if follow_up.generation != state.generation {
				continue;
			}
			let confirmed = state
				.confirmed_sends
				.iter()
				.any(|(nonce, _)| *nonce == follow_up.nonce);
			let sending = state.pending.iter().any(|pending| {
				pending.nonce == follow_up.nonce && pending.delivery == model::Delivery::Sending
			});
			if confirmed {
				match state.prepare_text_send_in(follow_up.channel, &follow_up.text) {
					Some(command) => commands.push(command),
					None => notices.push(crate::i18n::translate_args(
						"extensions-ui-hook-follow-up-unsent",
						&[("plugin", &follow_up.name)],
					)),
				}
			} else if sending {
				self.send.follow_ups.push(follow_up);
			} else {
				notices.push(crate::i18n::translate_args(
					"extensions-ui-hook-follow-up-unsent",
					&[("plugin", &follow_up.name)],
				));
			}
		}
		(commands, notices)
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
			let text = match &review.follow_up {
				Some(follow_up) => {
					let preview: String = follow_up.chars().take(200).collect();
					crate::i18n::translate_args(
						"extensions-ui-hook-review-follow-up",
						&[("plugin", &review.name), ("text", &preview)],
					)
				}
				None => crate::i18n::translate_args(
					"extensions-ui-hook-review",
					&[("plugin", &review.name)],
				),
			};
			(text, Some("extensions-ui-hook-use-original"))
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

	/// Switches `plugin`'s own hooks in `channel`, as the user confirmed from the plugin:
	/// its notes, and its send hook in `send` mode (which replaces another plugin's), or
	/// off. Saved like the composer menu's switches.
	pub(crate) fn set_plugin_hooks(
		&mut self,
		plugin: &str,
		channel: Id,
		notes: bool,
		send: Option<SendMode>,
	) -> Result<(), String> {
		let has = |this: &Self, surface| this.hook_plugins(surface).any(|(id, _, _)| id == plugin);
		if (notes && !has(self, Surface::Annotate)) || (send.is_some() && !has(self, Surface::Send))
		{
			return Err("That plugin has no hook for messages".into());
		}
		let before = self.hooks_in(channel);
		let mut hooks = before.clone();
		hooks.annotate.retain(|id| id != plugin);
		if notes {
			if hooks.annotate.len() >= extension_host::hooks::MAX_ANNOTATORS {
				return Err("Too many plugins add notes here; switch one off first".into());
			}
			hooks.annotate.push(plugin.to_owned());
		} else if before.annotate.iter().any(|id| id == plugin) {
			self.notes.push(NoteUpdate::Forget(plugin.to_owned()));
		}
		match send {
			Some(mode) => {
				// A fallback chosen in the menu stays.
				let fallback = hooks
					.send
					.as_ref()
					.filter(|hook| hook.plugin == plugin)
					.map_or(Fallback::KeepDraft, |hook| hook.fallback);
				hooks.send = Some(SendHook {
					plugin: plugin.to_owned(),
					mode,
					fallback,
				});
			}
			None if hooks
				.send
				.as_ref()
				.is_some_and(|hook| hook.plugin == plugin) =>
			{
				hooks.send = None;
			}
			None => {}
		}
		if hooks == before {
			return Ok(());
		}
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
		Ok(())
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
		let everywhere_before = self.hooks_in(extension_host::hooks::EVERYWHERE);
		let mut everywhere = everywhere_before.clone();
		for (id, name) in &annotators {
			let global = everywhere.annotate.contains(id);
			let mut on = global || hooks.annotate.contains(id);
			let room = |list: &[String]| list.len() < extension_host::hooks::MAX_ANNOTATORS;
			if ui
				.add_enabled(
					!global && (on || room(&hooks.annotate)),
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
			let mut all = global;
			ui.indent(("annotate-everywhere", id), |ui| {
				if ui
					.add_enabled(
						all || room(&everywhere.annotate),
						egui::Checkbox::new(
							&mut all,
							crate::i18n::translate("extensions-ui-hook-annotate-everywhere"),
						),
					)
					.changed()
				{
					everywhere.annotate.retain(|plugin| plugin != id);
					if all {
						everywhere.annotate.push(id.clone());
					} else if !hooks.annotate.contains(id) {
						self.notes.push(NoteUpdate::Forget(id.clone()));
					}
				}
			});
		}
		for (hooks, before) in [(hooks, before), (everywhere, everywhere_before)] {
			if hooks == before {
				continue;
			}
			// Shown at once; the saved list from the host replaces it.
			self.channel_hooks
				.retain(|saved| saved.channel != hooks.channel);
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
	pub fn receive_annotations(&mut self, id: &str, output: &extensions::Output) {
		let annotations: &[Annotation] = &output.annotations;
		let additions = annotations.len()
			+ output.histories.len()
			+ output.deleted_messages.len()
			+ output.alternates.len();
		if additions == 0 {
			return;
		}
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
		if self.notes.len() + additions > MAX_WAITING {
			let excess = self.notes.len() + additions - MAX_WAITING;
			let mut dropped = 0;
			self.notes.retain(|update| {
				let drop = dropped < excess
					&& matches!(
						update,
						NoteUpdate::Set { .. }
							| NoteUpdate::History { .. }
							| NoteUpdate::Deleted { .. }
							| NoteUpdate::Alternate { .. }
					);
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
		for alternate in &output.alternates {
			if let Ok(message) = alternate.message_id.parse() {
				self.notes.push(NoteUpdate::Alternate {
					plugin: id.to_owned(),
					name: name.clone(),
					message: Id(message),
					label: alternate.label.clone(),
					text: alternate.text.clone(),
				});
			}
		}
		for history in &output.histories {
			if let Ok(message) = history.message_id.parse() {
				self.notes.push(NoteUpdate::History {
					plugin: id.to_owned(),
					name: name.clone(),
					message: Id(message),
					history: history
						.revisions
						.iter()
						.map(|revision| extension_host::hooks::Revision {
							time_ms: revision.time_ms,
							text: revision.text.clone(),
						})
						.collect(),
				});
			}
		}
		for group in &output.deleted_messages {
			let Ok(anchor) = group.after_message_id.parse() else {
				continue;
			};
			let rows = group
				.messages
				.iter()
				.filter_map(|row| {
					Some(extension_host::hooks::Archived {
						message: Id(row.message_id.parse().ok()?),
						author: row.author.clone(),
						time_ms: row.time_ms,
						text: row.text.clone(),
						deleted_ms: row.deleted_ms,
						history: row
							.revisions
							.iter()
							.map(|revision| extension_host::hooks::Revision {
								time_ms: revision.time_ms,
								text: revision.text.clone(),
							})
							.collect(),
					})
				})
				.collect();
			self.notes.push(NoteUpdate::Deleted {
				plugin: id.to_owned(),
				name: name.clone(),
				message: Id(anchor),
				rows,
			});
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
				NoteUpdate::History {
					plugin,
					name,
					message,
					history,
				} => into.set_history(&plugin, &name, message, history),
				NoteUpdate::Deleted {
					plugin,
					name,
					message,
					rows,
				} => into.set_deleted(&plugin, &name, message, rows),
				NoteUpdate::Alternate {
					plugin,
					name,
					message,
					label,
					text,
				} => into.set_alternate(&plugin, &name, message, &label, &text),
				NoteUpdate::Forget(plugin) => into.remove_plugin(&plugin),
				NoteUpdate::Clear => into.clear(),
			}
		}
	}
}
