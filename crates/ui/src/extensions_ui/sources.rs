//! The catalog sources section of the extension gallery: added catalogs, each with an
//! optional token that the host keeps in the system credential store.
use super::{ExtensionRequest, ExtensionUi};
use crate::design;
use extension_host::sources::{CatalogSource, SourceStatus};
use zeroize::Zeroizing;

/// The add or edit form; `id` is empty for a new source.
#[derive(Default)]
pub(super) struct SourceForm {
	id: String,
	name: String,
	url: String,
	origins: String,
	auto_refresh: bool,
	token: Zeroizing<String>,
	forget_token: bool,
	has_token: bool,
}

impl SourceForm {
	fn new() -> Self {
		Self {
			auto_refresh: true,
			..Default::default()
		}
	}
	fn of(source: &CatalogSource) -> Self {
		Self {
			id: source.id.clone(),
			name: source.name.clone(),
			url: source.url.clone(),
			origins: source.token_origins.join(", "),
			auto_refresh: source.auto_refresh,
			has_token: source.has_token,
			..Default::default()
		}
	}
	fn request(&self) -> ExtensionRequest {
		let token = if !self.token.is_empty() {
			Some(Some(Zeroizing::new(self.token.trim().to_owned())))
		} else if self.forget_token {
			Some(None)
		} else {
			None
		};
		ExtensionRequest::SetCatalogSource {
			source: CatalogSource {
				id: self.id.clone(),
				name: self.name.trim().to_owned(),
				url: self.url.trim().to_owned(),
				token_origins: self
					.origins
					.split([',', ' ', '\n'])
					.filter(|origin| !origin.is_empty())
					.map(str::to_owned)
					.collect(),
				auto_refresh: self.auto_refresh,
				has_token: false,
			},
			token,
		}
	}
}

impl ExtensionUi {
	/// Lists the added catalog sources with their status, and the form to add or edit one.
	pub(super) fn catalog_sources(&mut self, ui: &mut egui::Ui, colors: &design::Palette) {
		ui.add_space(16.0);
		ui.label(design::eyebrow(
			ui,
			crate::i18n::translate("extensions-ui-sources-title"),
			colors.muted,
		));
		ui.add(
			egui::Label::new(
				egui::RichText::new(crate::i18n::translate("extensions-ui-sources-note"))
					.color(colors.muted),
			)
			.wrap(),
		);
		let mut request = None;
		let mut edit = None;
		for SourceStatus {
			source,
			entries,
			error,
		} in &self.catalog_sources
		{
			ui.horizontal_wrapped(|ui| {
				ui.label(egui::RichText::new(&source.name).color(colors.text));
				ui.label(egui::RichText::new(&source.url).small().color(colors.muted));
				match error {
					Some(error) => {
						ui.label(egui::RichText::new(error).small().color(colors.danger));
					}
					None => {
						ui.label(
							egui::RichText::new(crate::i18n::translate_args(
								"extensions-ui-sources-entries",
								&[("count", &entries.to_string())],
							))
							.small()
							.color(colors.muted),
						);
					}
				}
				if source.has_token {
					ui.label(
						egui::RichText::new(crate::i18n::translate(
							"extensions-ui-sources-token-set",
						))
						.small()
						.color(colors.positive),
					);
				}
				let busy = self.busy;
				if ui
					.add_enabled(
						!busy,
						egui::Button::new(crate::i18n::translate("extensions-ui-sources-refresh"))
							.small(),
					)
					.clicked()
				{
					request = Some(ExtensionRequest::RefreshCatalogSource {
						id: source.id.clone(),
					});
				}
				if ui
					.small_button(crate::i18n::translate("extensions-ui-sources-edit"))
					.clicked()
				{
					edit = Some(SourceForm::of(source));
				}
				if ui
					.add_enabled(
						!busy,
						egui::Button::new(crate::i18n::translate("extensions-ui-sources-remove"))
							.small(),
					)
					.clicked()
				{
					request = Some(ExtensionRequest::RemoveCatalogSource {
						id: source.id.clone(),
					});
				}
			});
		}
		if edit.is_some() {
			self.source_form = edit;
		}
		match self.source_form.as_mut() {
			None => {
				if self.catalog_sources.len() < extension_host::sources::MAX_SOURCES
					&& ui
						.button(crate::i18n::translate("extensions-ui-sources-add"))
						.clicked()
				{
					self.source_form = Some(SourceForm::new());
				}
			}
			Some(form) => {
				let mut close = false;
				egui::Frame::new()
					.inner_margin(8)
					.corner_radius(6)
					.stroke(egui::Stroke::new(1.0, colors.muted.gamma_multiply(0.3)))
					.show(ui, |ui| {
						let field =
							|ui: &mut egui::Ui, label: &str, value: &mut String, password| {
								let label = ui.label(crate::i18n::translate(label));
								ui.add(
									egui::TextEdit::singleline(value)
										.password(password)
										.char_limit(4096)
										.desired_width(ui.available_width().min(420.0)),
								)
								.labelled_by(label.id);
							};
						field(ui, "extensions-ui-sources-name", &mut form.name, false);
						field(ui, "extensions-ui-sources-url", &mut form.url, false);
						field(ui, "extensions-ui-sources-token", &mut form.token, true);
						if form.has_token {
							ui.checkbox(
								&mut form.forget_token,
								crate::i18n::translate("extensions-ui-sources-forget-token"),
							);
						}
						field(
							ui,
							"extensions-ui-sources-origins",
							&mut form.origins,
							false,
						);
						ui.checkbox(
							&mut form.auto_refresh,
							crate::i18n::translate("extensions-ui-sources-auto"),
						);
						ui.horizontal(|ui| {
							if ui
								.add_enabled(
									!self.busy,
									egui::Button::new(crate::i18n::translate(
										"extensions-ui-sources-save",
									)),
								)
								.clicked()
							{
								request = Some(form.request());
								close = true;
							}
							if ui
								.button(crate::i18n::translate("extensions-ui-sources-cancel"))
								.clicked()
							{
								close = true;
							}
						});
					});
				if close {
					self.source_form = None;
				}
			}
		}
		if let Some(request) = request {
			self.queue(ui.ctx(), request);
		}
	}
	/// "from <source>" for an entry found in an added catalog.
	pub(super) fn source_label(&self, id: &str) -> Option<String> {
		self.catalog_origins.get(id).map(|name| {
			crate::i18n::translate_args("extensions-ui-sources-from", &[("source", name)])
		})
	}
}
