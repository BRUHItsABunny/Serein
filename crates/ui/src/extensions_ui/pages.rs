//! Plugin pages in the main area: the plugin's panel elements above a list the host draws a
//! screenful at a time, fetching rows it lacks from the plugin's `rows_action`.
use super::{ExtensionContext, ExtensionRequest, ExtensionUi, render_elements};
use crate::design;
use client_core::State;
use extensions::{Invocation, Output, Page, PageRequest, PageRow, PageRowState};
use model::Id;
use std::collections::BTreeMap;

/// Rows fetched in one request.
const CHUNK: u32 = 50;
/// Rows fetched beyond the visible ones in each direction.
const MARGIN: u32 = 50;
/// Fetched rows kept; the farthest from view go first.
const MAX_ROWS: usize = 2_000;
const MAX_ROW_BYTES: usize = 8 * 1024 * 1024;
const ROW_HEIGHT: f32 = 58.0;

pub(crate) struct OpenPage {
	pub(super) plugin: String,
	page: Page,
	values: BTreeMap<String, String>,
	rows: BTreeMap<u32, PageRow>,
	bytes: usize,
	/// The first row of the request in flight.
	requested: Option<u32>,
	/// The conversation open when the page opened; opening another closes it.
	selected: Option<Id>,
	visible: std::ops::Range<u32>,
}

fn row_bytes(row: &PageRow) -> usize {
	size_of::<PageRow>()
		+ row.id.len()
		+ row.author.as_ref().map_or(0, String::len)
		+ row.text.len()
		+ row
			.revisions
			.iter()
			.map(|revision| size_of::<extensions::PageRevision>() + revision.text.len())
			.sum::<usize>()
}

impl OpenPage {
	fn version(&self) -> Option<&str> {
		self.page.list.as_ref().map(|list| list.version.as_str())
	}
	fn total(&self) -> u32 {
		self.page.list.as_ref().map_or(0, |list| list.total)
	}
	fn insert(&mut self, start: u32, rows: Vec<PageRow>) {
		let total = self.total();
		for (offset, row) in (start..total).zip(rows) {
			self.bytes += row_bytes(&row);
			if let Some(old) = self.rows.insert(offset, row) {
				self.bytes -= row_bytes(&old);
			}
		}
		// Keep what is on screen; drop the rows farthest from it.
		let middle = self.visible.start / 2 + self.visible.end / 2;
		while self.rows.len() > MAX_ROWS || self.bytes > MAX_ROW_BYTES {
			let (first, last) = match (self.rows.first_key_value(), self.rows.last_key_value()) {
				(Some((first, _)), Some((last, _))) => (*first, *last),
				_ => break,
			};
			let far = if middle.abs_diff(first) > middle.abs_diff(last) {
				first
			} else {
				last
			};
			if let Some(row) = self.rows.remove(&far) {
				self.bytes -= row_bytes(&row);
			}
		}
	}
	/// The first chunk near the visible rows that has not been fetched.
	fn missing(&self) -> Option<u32> {
		let total = self.total();
		let from = self.visible.start.saturating_sub(MARGIN) / CHUNK * CHUNK;
		let to = self.visible.end.saturating_add(MARGIN).min(total);
		(from..to).step_by(CHUNK as usize).find(|start| {
			(*start..(*start + CHUNK).min(total)).any(|row| !self.rows.contains_key(&row))
		})
	}
}

impl ExtensionUi {
	/// Takes a result's page and rows. Returns whether the result is fully handled, so no
	/// result window opens.
	pub(super) fn receive_page(
		&mut self,
		id: &str,
		invocation: &Invocation,
		output: &mut Output,
		state: &State,
	) -> bool {
		let fetch = invocation.page_request.is_some();
		if fetch && let Some(open) = self.page.as_mut().filter(|open| open.plugin == id) {
			open.requested = None;
		}
		let mut handled = fetch;
		match output.page.take() {
			Some(Some(page)) => {
				let name = self
					.entries
					.iter()
					.find(|entry| entry.manifest.id == id)
					.map(|entry| entry.manifest.name.clone());
				if name.is_some() {
					let kept = self.page.take().filter(|open| {
						open.plugin == id
							&& open.version().is_some()
							&& open.version()
								== page.list.as_ref().map(|list| list.version.as_str())
					});
					let inline = page
						.list
						.as_ref()
						.map(|list| list.rows.clone())
						.unwrap_or_default();
					let mut open = match kept {
						Some(mut open) => {
							open.page = page;
							open.values.clear();
							open
						}
						None => OpenPage {
							plugin: id.to_owned(),
							page,
							values: BTreeMap::new(),
							rows: BTreeMap::new(),
							bytes: 0,
							requested: None,
							selected: state.selected,
							visible: 0..0,
						},
					};
					if let Some(list) = open.page.list.as_mut() {
						list.rows.clear();
					}
					open.insert(0, inline);
					self.page = Some(open);
				}
				handled = true;
			}
			Some(None) => {
				if self.page.as_ref().is_some_and(|open| open.plugin == id) {
					self.page = None;
				}
				handled = true;
			}
			None => {}
		}
		if let Some(rows) = output.page_rows.take()
			&& let Some(open) = self.page.as_mut().filter(|open| open.plugin == id)
			&& open.version() == Some(rows.version.as_str())
		{
			open.insert(rows.start, rows.rows);
		}
		handled && output.panel.is_empty() && output.effects.is_empty()
	}
	/// A row fetch that failed or was dropped may be asked for again.
	pub fn page_failed(&mut self, id: &str, invocation: &Invocation) {
		if invocation.page_request.is_some()
			&& let Some(open) = self.page.as_mut().filter(|open| open.plugin == id)
		{
			open.requested = None;
		}
	}
	/// Whether a page shows; opening another conversation or the plugin stopping closes it.
	pub(crate) fn page_open(&mut self, state: &State) -> bool {
		if self.page.as_ref().is_some_and(|open| {
			open.selected != state.selected
				|| !self
					.entries
					.iter()
					.any(|entry| entry.manifest.id == open.plugin && entry.enabled)
		}) {
			self.page = None;
		}
		self.page.is_some()
	}
	/// Draws the open page into the main area.
	pub(crate) fn show_page(&mut self, ui: &mut egui::Ui, state: &State) {
		let Some(open) = self.page.as_mut() else {
			return;
		};
		let colors = design::palette(ui);
		let mut close = false;
		let mut action = None;
		let mut clicked = None;
		egui::Frame::new()
			.inner_margin(egui::Margin::symmetric(16, 12))
			.show(ui, |ui| {
				ui.horizontal(|ui| {
					ui.label(design::semibold(ui, &open.page.title, 18.0));
					ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
						close = ui
							.button(crate::i18n::translate("extensions-ui-page-close"))
							.clicked();
					});
				});
				if !open.page.elements.is_empty() {
					ui.add_space(6.0);
					let elements = open.page.elements.clone();
					render_elements(ui, state, &elements, &mut open.values, &mut action);
				}
				ui.add_space(8.0);
				ui.separator();
				let Some(list) = open.page.list.as_ref() else {
					return;
				};
				if list.total == 0 {
					ui.add_space(24.0);
					ui.vertical_centered(|ui| {
						ui.weak(
							list.empty_text.clone().unwrap_or_else(|| {
								crate::i18n::translate("extensions-ui-page-empty")
							}),
						)
					});
					return;
				}
				let total = list.total;
				let clickable = list.click_action.is_some();
				let mut visible = 0..0;
				egui::ScrollArea::vertical().auto_shrink(false).show_rows(
					ui,
					ROW_HEIGHT,
					total as usize,
					|ui, range| {
						visible = range.start as u32..range.end as u32;
						for index in range {
							let row = open.rows.get(&(index as u32));
							if let Some(id) = page_row(ui, row, clickable, &colors) {
								clicked = Some(id);
							}
						}
					},
				);
				open.visible = visible;
			});
		if close {
			self.page = None;
			return;
		}
		let Some(open) = self.page.as_ref() else {
			return;
		};
		let plugin = open.plugin.clone();
		let values = open.values.clone();
		let click_action = open
			.page
			.list
			.as_ref()
			.and_then(|list| list.click_action.clone());
		if let Some(action) = action
			&& self.panel_action_declared(&plugin, &action)
		{
			self.queue(
				ui.ctx(),
				ExtensionRequest::Invoke {
					id: plugin.clone(),
					invocation: Invocation {
						action,
						values: values.clone(),
						..Default::default()
					},
					context: ExtensionContext::panel(state),
				},
			);
		}
		if let Some(row) = clicked
			&& let Some(action) = click_action
			&& self.panel_action_declared(&plugin, &action)
		{
			let mut values = values;
			values.insert("row".into(), row);
			self.queue(
				ui.ctx(),
				ExtensionRequest::Invoke {
					id: plugin.clone(),
					invocation: Invocation {
						action,
						values,
						..Default::default()
					},
					context: ExtensionContext::panel(state),
				},
			);
		}
		self.fetch_rows(ui.ctx(), state);
	}
	/// Asks for the first missing chunk near the visible rows, one request at a time.
	fn fetch_rows(&mut self, ctx: &egui::Context, state: &State) {
		let Some(open) = self.page.as_ref() else {
			return;
		};
		let Some(list) = open.page.list.as_ref() else {
			return;
		};
		if open.requested.is_some() || self.busy || !self.requests.is_empty() {
			return;
		}
		let Some(start) = open.missing() else {
			return;
		};
		let request = PageRequest {
			version: list.version.clone(),
			start,
			count: CHUNK.min(list.total - start),
		};
		let (plugin, action) = (open.plugin.clone(), list.rows_action.clone());
		if !self.panel_action_declared(&plugin, &action) {
			return;
		}
		self.queue(
			ctx,
			ExtensionRequest::Invoke {
				id: plugin,
				invocation: Invocation {
					action,
					page_request: Some(Box::new(request)),
					..Default::default()
				},
				context: ExtensionContext::panel(state),
			},
		);
		if let Some(open) = self.page.as_mut() {
			open.requested = Some(start);
		}
	}
}

/// One list row at the fixed row height; returns its id when clicked.
fn page_row(
	ui: &mut egui::Ui,
	row: Option<&PageRow>,
	clickable: bool,
	colors: &design::Palette,
) -> Option<String> {
	let (rect, response) = ui.allocate_exact_size(
		egui::vec2(ui.available_width(), ROW_HEIGHT),
		if clickable && row.is_some() {
			egui::Sense::click()
		} else {
			egui::Sense::hover()
		},
	);
	let Some(row) = row else {
		ui.painter().rect_filled(
			rect.shrink2(egui::vec2(0.0, 10.0)),
			4,
			colors.muted.gamma_multiply(0.08),
		);
		return None;
	};
	if response.hovered() {
		ui.painter()
			.rect_filled(rect, 4, colors.muted.gamma_multiply(0.06));
	}
	let deleted = row.state == Some(PageRowState::Deleted);
	let mut child = ui.new_child(
		egui::UiBuilder::new()
			.max_rect(rect.shrink2(egui::vec2(8.0, 6.0)))
			.layout(egui::Layout::top_down(egui::Align::Min)),
	);
	child.horizontal(|ui| {
		if let Some(author) = &row.author {
			ui.label(design::semibold(ui, author, 14.0));
		}
		if let Some(time) = row.time_ms.and_then(utc) {
			ui.weak(crate::local_time::header(time));
		}
		if deleted {
			ui.label(
				egui::RichText::new(crate::i18n::translate("extensions-ui-page-deleted"))
					.small()
					.color(colors.danger),
			);
		}
		if !row.revisions.is_empty() {
			history(ui, row);
		} else if row.state == Some(PageRowState::Edited) {
			ui.weak(
				egui::RichText::new(crate::i18n::translate("extensions-ui-page-edited")).small(),
			);
		}
	});
	let first = row.text.lines().next().unwrap_or_default();
	let mut text = egui::RichText::new(first);
	if deleted {
		text = text.strikethrough().color(colors.muted);
	}
	let label = child.add(egui::Label::new(text).truncate());
	if row.text.len() > first.len() || label.hovered() {
		label.on_hover_text(&row.text);
	}
	(clickable && response.clicked()).then(|| row.id.clone())
}

/// An "edited" button opening the row's earlier versions.
fn history(ui: &mut egui::Ui, row: &PageRow) {
	let button = ui.add(
		egui::Button::new(
			egui::RichText::new(crate::i18n::translate("extensions-ui-page-history")).small(),
		)
		.frame(false),
	);
	egui::Popup::from_toggle_button_response(&button).show(|ui| {
		ui.set_max_width(420.0);
		egui::ScrollArea::vertical()
			.max_height(320.0)
			.show(ui, |ui| {
				for revision in &row.revisions {
					if let Some(time) = revision.time_ms.and_then(utc) {
						ui.weak(crate::local_time::full(time));
					}
					ui.add(egui::Label::new(&revision.text).wrap());
					ui.separator();
				}
				ui.weak(crate::i18n::translate("extensions-ui-page-current"));
				ui.add(egui::Label::new(&row.text).wrap());
			});
	});
}

fn utc(ms: i64) -> Option<time::OffsetDateTime> {
	time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok()
}

#[cfg(test)]
mod tests {
	use super::*;
	use extensions::{Action, Capability, PageList, PageRows, Surface};

	fn row(id: u32) -> PageRow {
		PageRow {
			id: id.to_string(),
			author: None,
			time_ms: None,
			text: format!("message {id}"),
			state: None,
			revisions: Vec::new(),
		}
	}
	fn page(version: &str, total: u32, inline: u32) -> Output {
		Output {
			page: Some(Some(Page {
				title: "Archive".into(),
				elements: Vec::new(),
				list: Some(PageList {
					version: version.into(),
					total,
					rows_action: "rows".into(),
					click_action: None,
					rows: (0..inline).map(row).collect(),
					empty_text: None,
				}),
			})),
			..Default::default()
		}
	}
	/// The request the page queued last, as (start, count).
	fn requested(ui: &mut ExtensionUi) -> Option<(u32, u32)> {
		match ui.requests.pop()? {
			ExtensionRequest::Invoke { invocation, .. } => invocation
				.page_request
				.map(|request| (request.start, request.count)),
			_ => None,
		}
	}

	/// The page fetches one missing chunk at a time around what is visible, ignores rows of
	/// an older version, keeps fetched rows when restated with the same version and drops
	/// them for a new one, and evicts the rows farthest from view past its bound.
	#[test]
	fn pages_fetch_visible_rows_once_and_drop_stale_or_distant_ones() {
		let state = test_support::demo_state();
		let ctx = egui::Context::default();
		let mut ui = ExtensionUi::default();
		let mut entry = super::super::tests::entry();
		entry.enabled = true;
		entry.manifest.capabilities = vec![Capability::Pages];
		entry.manifest.actions = vec![Action {
			id: "rows".into(),
			label: "Rows".into(),
			surface: Surface::Panel,
		}];
		ui.entries = vec![entry];
		let id = "synthetic.words";
		let open = Invocation {
			action: "rows".into(),
			..Default::default()
		};
		assert!(ui.receive_page(id, &open, &mut page("v1", 500, 10), &state));
		assert!(ui.page_open(&state));

		ui.page.as_mut().unwrap().visible = 0..12;
		ui.fetch_rows(&ctx, &state);
		assert_eq!(requested(&mut ui), Some((0, 50)), "rows 10..50 are missing");
		ui.fetch_rows(&ctx, &state);
		assert_eq!(requested(&mut ui), None, "one request at a time");

		let fetch = |start| Invocation {
			action: "rows".into(),
			page_request: Some(Box::new(PageRequest {
				version: "v1".into(),
				start,
				count: 50,
			})),
			..Default::default()
		};
		let rows = |version: &str, start: u32| Output {
			page_rows: Some(PageRows {
				version: version.into(),
				start,
				rows: (start..start + 50).map(row).collect(),
			}),
			..Default::default()
		};
		// A late answer for an older version only ends the request.
		assert!(ui.receive_page(id, &fetch(0), &mut rows("v0", 0), &state));
		ui.fetch_rows(&ctx, &state);
		assert_eq!(requested(&mut ui), Some((0, 50)));
		assert!(ui.receive_page(id, &fetch(0), &mut rows("v1", 0), &state));
		ui.fetch_rows(&ctx, &state);
		assert_eq!(
			requested(&mut ui),
			Some((50, 50)),
			"the margin past the view"
		);
		assert!(ui.receive_page(id, &fetch(50), &mut rows("v1", 50), &state));
		ui.fetch_rows(&ctx, &state);
		assert_eq!(
			requested(&mut ui),
			None,
			"everything near the view is there"
		);

		// The same version keeps the fetched rows; a new one starts over.
		assert!(ui.receive_page(id, &open, &mut page("v1", 500, 0), &state));
		assert_eq!(ui.page.as_ref().unwrap().rows.len(), 100);
		assert!(ui.receive_page(id, &open, &mut page("v2", 500, 0), &state));
		assert!(ui.page.as_ref().unwrap().rows.is_empty());

		// Past the bound, the rows farthest from view are dropped.
		let open_page = ui.page.as_mut().unwrap();
		open_page.page.list.as_mut().unwrap().total = MAX_ROWS as u32 + 100;
		open_page.visible = 0..10;
		open_page.insert(0, (0..MAX_ROWS as u32 + 100).map(row).collect());
		assert_eq!(open_page.rows.len(), MAX_ROWS);
		assert!(
			open_page.rows.contains_key(&0)
				&& !open_page.rows.contains_key(&(MAX_ROWS as u32 + 99))
		);

		// A page closes itself with `null`, and when the user opens another conversation.
		assert!(ui.receive_page(
			id,
			&open,
			&mut Output {
				page: Some(None),
				..Default::default()
			},
			&state
		));
		assert!(!ui.page_open(&state));
		ui.receive_page(id, &open, &mut page("v1", 1, 1), &state);
		let mut moved = test_support::demo_state();
		moved.selected = moved
			.channels
			.iter()
			.map(|channel| channel.id)
			.find(|id| Some(*id) != state.selected);
		assert!(!ui.page_open(&moved));
	}
}
