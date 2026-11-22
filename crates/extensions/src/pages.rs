//! Plugin pages: a host-drawn page in the main area with the plugin's panel elements above a
//! long list whose rows the host fetches as the user scrolls. Rows are message-like (author,
//! time, text) and may be marked edited or deleted, with earlier versions the host shows in
//! an edit-history popover. Nothing in a page is sent to Discord.
use crate::{Element, Error, MAX_PANEL_ELEMENTS};
use serde::{Deserialize, Serialize};

/// Largest page title, in UTF-8 bytes.
pub const MAX_PAGE_TITLE_BYTES: usize = 128;
/// Most rows a list may report.
pub const MAX_PAGE_LIST_ROWS: u32 = 1_000_000;
/// Most rows in one fetch, and inline in a page.
pub const MAX_PAGE_FETCH_ROWS: usize = 100;
/// Largest row id, in UTF-8 bytes.
pub const MAX_PAGE_ROW_ID_BYTES: usize = 64;
/// Largest row author, in UTF-8 bytes.
pub const MAX_PAGE_AUTHOR_BYTES: usize = 128;
/// Largest row text or revision text, in UTF-8 bytes.
pub const MAX_PAGE_ROW_TEXT_BYTES: usize = 4 * 1024;
/// Most earlier versions of one row.
pub const MAX_PAGE_REVISIONS: usize = 8;
/// Largest list version tag, in UTF-8 bytes.
pub const MAX_PAGE_VERSION_BYTES: usize = 64;

/// A page shown in the main area until the user closes it or opens a conversation.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
	pub title: String,
	/// Panel elements above the list; buttons run the panel action they name, with the
	/// page's input values, like buttons in a result window.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub elements: Vec<Element>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub list: Option<PageList>,
}

/// A list the host draws a screenful at a time, fetching rows it has not got through
/// `rows_action` with a [`PageRequest`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageList {
	/// Names the list's current content; a new version drops the rows the host fetched.
	pub version: String,
	pub total: u32,
	/// The declared panel action that returns rows.
	pub rows_action: String,
	/// The declared panel action run when a row is clicked, with its id in `values["row"]`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub click_action: Option<String>,
	/// The first rows, so a short list needs no fetch.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub rows: Vec<PageRow>,
	/// Shown when `total` is zero.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub empty_text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageRowState {
	Edited,
	Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRow {
	pub id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub author: Option<String>,
	/// Unix time in milliseconds, shown in the user's local time.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub time_ms: Option<i64>,
	pub text: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub state: Option<PageRowState>,
	/// Earlier versions, oldest first, for the edit-history popover.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub revisions: Vec<PageRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRevision {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub time_ms: Option<i64>,
	pub text: String,
}

/// The rows the host wants, on a `rows_action` invocation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRequest {
	pub version: String,
	pub start: u32,
	pub count: u32,
}

/// Rows answering a [`PageRequest`], starting at its `start`; fewer than asked is fine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageRows {
	pub version: String,
	pub start: u32,
	pub rows: Vec<PageRow>,
}

fn line(text: &str, bytes: usize) -> Result<(), Error> {
	if text.len() > bytes {
		return Err(Error::Limit);
	}
	if text.trim().is_empty() || text.chars().any(char::is_control) {
		return Err(Error::Invalid);
	}
	Ok(())
}

/// Row and revision text may span lines; other control characters are refused.
fn body(text: &str) -> Result<(), Error> {
	if text.len() > MAX_PAGE_ROW_TEXT_BYTES {
		return Err(Error::Limit);
	}
	if text
		.chars()
		.any(|character| character.is_control() && character != '\n')
	{
		return Err(Error::Invalid);
	}
	Ok(())
}

fn version(tag: &str) -> Result<(), Error> {
	if tag.is_empty() || tag.len() > MAX_PAGE_VERSION_BYTES || !tag.is_ascii() {
		return Err(Error::Invalid);
	}
	line(tag, MAX_PAGE_VERSION_BYTES)
}

fn rows(rows: &[PageRow], start: u32, total: u32) -> Result<(), Error> {
	if rows.len() > MAX_PAGE_FETCH_ROWS {
		return Err(Error::Limit);
	}
	if u64::from(start) + rows.len() as u64 > u64::from(total) {
		return Err(Error::Invalid);
	}
	for row in rows {
		line(&row.id, MAX_PAGE_ROW_ID_BYTES)?;
		if let Some(author) = &row.author {
			line(author, MAX_PAGE_AUTHOR_BYTES)?;
		}
		body(&row.text)?;
		if row.revisions.len() > MAX_PAGE_REVISIONS {
			return Err(Error::Limit);
		}
		for revision in &row.revisions {
			body(&revision.text)?;
		}
	}
	Ok(())
}

impl Page {
	/// `panel_action` says whether the plugin declares a panel action with this id.
	pub fn validate(&self, panel_action: impl Fn(&str) -> bool) -> Result<(), Error> {
		line(&self.title, MAX_PAGE_TITLE_BYTES)?;
		if self.elements.len() > MAX_PANEL_ELEMENTS {
			return Err(Error::Limit);
		}
		let Some(list) = &self.list else {
			return Ok(());
		};
		version(&list.version)?;
		if list.total > MAX_PAGE_LIST_ROWS {
			return Err(Error::Limit);
		}
		if !panel_action(&list.rows_action)
			|| list
				.click_action
				.as_deref()
				.is_some_and(|action| !panel_action(action))
		{
			return Err(Error::Capability);
		}
		if let Some(text) = &list.empty_text {
			line(text, MAX_PAGE_ROW_TEXT_BYTES)?;
		}
		rows(&list.rows, 0, list.total)
	}
}

impl PageRequest {
	pub fn validate(&self) -> Result<(), Error> {
		version(&self.version)?;
		if self.count == 0
			|| self.count as usize > MAX_PAGE_FETCH_ROWS
			|| self.start >= MAX_PAGE_LIST_ROWS
		{
			return Err(Error::Limit);
		}
		Ok(())
	}
}

impl PageRows {
	/// Answers `request`: same version and start, no more rows than asked for.
	pub fn validate(&self, request: &PageRequest) -> Result<(), Error> {
		if self.version != request.version
			|| self.start != request.start
			|| self.rows.len() > request.count as usize
		{
			return Err(Error::Invalid);
		}
		rows(&self.rows, self.start, MAX_PAGE_LIST_ROWS)
	}
}

pub(crate) fn present<'de, D>(deserializer: D) -> Result<Option<Option<Page>>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	Option::<Page>::deserialize(deserializer).map(Some)
}
