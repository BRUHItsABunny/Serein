//! Plugin pages (preview): a page in the main area with panel elements above a long list the
//! host draws a screenful at a time, asking the plugin for rows as the user scrolls.
use crate::{Element, Invocation, Output};
use serde::{Deserialize, Serialize};

/// Most rows a list may report.
pub const MAX_PAGE_LIST_ROWS: u32 = 1_000_000;
/// Most rows in one fetch, and inline in a page.
pub const MAX_PAGE_FETCH_ROWS: usize = 100;
/// Largest row text or revision text, in UTF-8 bytes; `\n` is allowed.
pub const MAX_PAGE_ROW_TEXT_BYTES: usize = 4 * 1024;
/// Most earlier versions of one row.
pub const MAX_PAGE_REVISIONS: usize = 8;

/// A page shown in the main area until the user closes it or opens a conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
	/// At most 128 bytes.
	pub title: String,
	/// Panel elements above the list; buttons run the panel action they name.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub elements: Vec<Element>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub list: Option<PageList>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageList {
	/// Names the current content (1–64 ASCII bytes); change it when rows change, so the host
	/// drops what it fetched.
	pub version: String,
	pub total: u32,
	/// A declared panel action that answers [`PageInvocation::page_request`] with
	/// [`PageOutput::page_rows`].
	pub rows_action: String,
	/// A declared panel action run when a row is clicked, with its id in `values["row"]`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub click_action: Option<String>,
	/// The first rows, so a short list needs no fetch.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub rows: Vec<PageRow>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub empty_text: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageRowState {
	Edited,
	Deleted,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRow {
	/// 1–64 bytes.
	pub id: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub author: Option<String>,
	/// Unix time in milliseconds.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub time_ms: Option<i64>,
	pub text: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub state: Option<PageRowState>,
	/// Earlier versions, oldest first, shown in the edit-history popover.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub revisions: Vec<PageRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRevision {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub time_ms: Option<i64>,
	pub text: String,
}

/// The rows the host wants: `count` (1–100) rows from `start` of list `version`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRequest {
	pub version: String,
	pub start: u32,
	pub count: u32,
}

/// Rows from `request.start`; return fewer if they would not fit the 256 KiB output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRows {
	pub version: String,
	pub start: u32,
	pub rows: Vec<PageRow>,
}

/// Input of a panel action that may carry a [`PageRequest`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageInvocation {
	#[serde(flatten)]
	pub invocation: Invocation,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub page_request: Option<PageRequest>,
}

/// Output that opens, replaces or closes this plugin's page, or answers a row request.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageOutput {
	#[serde(flatten)]
	pub output: Output,
	/// `Some(Some(page))` opens or replaces the page, `Some(None)` closes it, `None` keeps it.
	#[serde(default, skip_serializing_if = "Option::is_none", with = "nullable")]
	pub page: Option<Option<Page>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub page_rows: Option<PageRows>,
}

mod nullable {
	use serde::{Deserialize, Deserializer, Serialize, Serializer};
	pub fn serialize<S: Serializer, T: Serialize>(
		value: &Option<Option<T>>,
		serializer: S,
	) -> Result<S::Ok, S::Error> {
		match value {
			Some(inner) => inner.serialize(serializer),
			None => serializer.serialize_none(),
		}
	}
	pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
		deserializer: D,
	) -> Result<Option<Option<T>>, D::Error> {
		Option::<T>::deserialize(deserializer).map(Some)
	}
}
