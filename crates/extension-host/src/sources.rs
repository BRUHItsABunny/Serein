//! Catalog sources the user added beside the built-in catalog, such as a plugin repository's
//! release catalog. Each source may hold a token, kept in the OS secret store and sent only
//! to the source's own origin and the origins the user listed for it; a redirect elsewhere
//! drops it. Entries keep their pinned hashes, and the built-in catalog wins an ID clash.
use extensions::{Catalog, network::Origin};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Sources one device may add.
pub const MAX_SOURCES: usize = 8;
/// Extra origins one source's token may be sent to.
pub const MAX_TOKEN_ORIGINS: usize = 8;
pub(crate) const SOURCES_FILE: &str = "sources.json";
const MAX_SOURCES_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSource {
	/// Lowercase letters, digits and dashes, chosen by the host.
	pub id: String,
	pub name: String,
	/// The catalog JSON, over HTTPS.
	pub url: String,
	/// Origins besides the catalog's own that the token may be sent to, such as where its
	/// packages download from.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub token_origins: Vec<String>,
	/// Refresh with the built-in catalog; otherwise only on request.
	#[serde(default = "yes")]
	pub auto_refresh: bool,
	/// A token is saved in the secret store.
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub has_token: bool,
}

fn yes() -> bool {
	true
}

impl CatalogSource {
	pub fn validate(&self) -> Result<(), String> {
		crate::store::valid_id(&self.id)?;
		if self.name.trim().is_empty()
			|| self.name.len() > 64
			|| self.name.chars().any(char::is_control)
		{
			return Err("Give the source a short name".into());
		}
		self.catalog_origin()
			.ok_or("The catalog address must be a public HTTPS URL")?;
		if self.token_origins.len() > MAX_TOKEN_ORIGINS
			|| self
				.token_origins
				.iter()
				.any(|origin| Origin::parse(origin).is_none())
		{
			return Err(format!(
				"List up to {MAX_TOKEN_ORIGINS} token origins such as https://api.github.com"
			));
		}
		Ok(())
	}
	fn catalog_origin(&self) -> Option<Origin> {
		let url = url::Url::parse(&self.url).ok()?;
		extensions::valid_https_url(&self.url)
			.then(|| Origin::of(&url))
			.flatten()
	}
	/// Where the token may go: the catalog's origin and the listed ones.
	pub fn token_scope(&self) -> Vec<Origin> {
		self.catalog_origin()
			.into_iter()
			.chain(
				self.token_origins
					.iter()
					.filter_map(|origin| Origin::parse(origin)),
			)
			.collect()
	}
}

/// A source with what its last refresh found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceStatus {
	pub source: CatalogSource,
	pub entries: usize,
	pub error: Option<String>,
}

/// A token and the origins it may be sent to.
pub(crate) struct Auth {
	pub token: zeroize::Zeroizing<String>,
	pub origins: Vec<Origin>,
}

impl Auth {
	/// The token, if `url` is one of its origins.
	pub fn for_url(&self, url: &url::Url) -> Option<&str> {
		let origin = Origin::of(url)?;
		self.origins
			.contains(&origin)
			.then_some(self.token.as_str())
	}
}

pub(crate) fn token_key(id: &str) -> String {
	crate::secrets::key("catalog", id, "token")
}

pub(crate) fn cache_path(root: &Path, id: &str) -> PathBuf {
	root.join("sources").join(format!("{id}.json"))
}

pub(crate) fn read(root: &Path) -> Vec<CatalogSource> {
	crate::store::read_bounded(&root.join(SOURCES_FILE), MAX_SOURCES_BYTES)
		.ok()
		.and_then(|bytes| serde_json::from_slice::<Vec<CatalogSource>>(&bytes).ok())
		.map(|sources| {
			sources
				.into_iter()
				.filter(|source| source.validate().is_ok())
				.take(MAX_SOURCES)
				.collect()
		})
		.unwrap_or_default()
}

/// The source's cached catalog, if it parses.
pub(crate) fn cached(root: &Path, id: &str) -> Option<Catalog> {
	let bytes =
		crate::store::read_bounded(&cache_path(root, id), extensions::MAX_CATALOG_BYTES).ok()?;
	extensions::parse_catalog(&bytes).ok()
}

/// The built-in catalog's entries, then each source's in order, each entry tagged with its
/// source. An ID already listed is skipped, so a source cannot replace a built-in entry.
pub fn merge(official: Option<Catalog>, sources: Vec<(String, Catalog)>) -> Option<Catalog> {
	let mut merged = match official {
		Some(catalog) => catalog,
		None if sources.is_empty() => return None,
		None => Catalog {
			api_version: extensions::API_VERSION,
			entries: Vec::new(),
		},
	};
	for entry in &mut merged.entries {
		entry.catalog_source = None;
	}
	for (id, catalog) in sources {
		for mut entry in catalog.entries {
			if merged
				.entries
				.iter()
				.any(|known| known.manifest.id == entry.manifest.id)
			{
				continue;
			}
			entry.catalog_source = Some(id.clone());
			merged.entries.push(entry);
		}
	}
	Some(merged)
}

/// A fresh source id from its name.
pub fn new_id(name: &str, existing: &[CatalogSource]) -> String {
	let base: String = name
		.to_ascii_lowercase()
		.chars()
		.map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
		.collect::<String>()
		.split('-')
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>()
		.join("-")
		.chars()
		.take(32)
		.collect();
	let base = if base.is_empty() {
		"source".to_owned()
	} else {
		base
	};
	let mut id = base.clone();
	let mut n = 2;
	while existing.iter().any(|source| source.id == id) {
		id = format!("{base}-{n}");
		n += 1;
	}
	id
}

#[cfg(test)]
mod tests {
	use super::*;

	fn source(url: &str, origins: &[&str]) -> CatalogSource {
		CatalogSource {
			id: "team".into(),
			name: "Team plugins".into(),
			url: url.into(),
			token_origins: origins.iter().map(|origin| origin.to_string()).collect(),
			auto_refresh: true,
			has_token: true,
		}
	}

	/// A source needs a public HTTPS catalog and exact origins; its token goes only to the
	/// catalog's origin and the listed ones, so each redirect hop to anywhere else, such as
	/// GitHub's signed asset storage, loses it.
	#[test]
	fn tokens_go_only_to_the_sources_own_origins() {
		let team = source(
			"https://api.github.com/repos/team/plugins/contents/catalog.json",
			&["https://uploads.example.org"],
		);
		team.validate().unwrap();
		for invalid in [
			source("http://example.org/catalog.json", &[]),
			source("https://10.0.0.1/catalog.json", &[]),
			source(
				"https://example.org/catalog.json",
				&["https://example.org/path"],
			),
			source("https://example.org/catalog.json", &["*.example.org"]),
			source(
				"https://example.org/catalog.json",
				&["https://a.example.org"; MAX_TOKEN_ORIGINS + 1],
			),
		] {
			assert!(invalid.validate().is_err(), "{invalid:?}");
		}
		let auth = Auth {
			token: zeroize::Zeroizing::new("secret".into()),
			origins: team.token_scope(),
		};
		let url = |text: &str| url::Url::parse(text).unwrap();
		assert_eq!(
			auth.for_url(&url(
				"https://api.github.com/repos/team/plugins/releases/assets/1"
			)),
			Some("secret")
		);
		assert_eq!(
			auth.for_url(&url("https://uploads.example.org/p.serein-extension")),
			Some("secret")
		);
		for elsewhere in [
			"https://objects.githubusercontent.com/github-production-release-asset/1",
			"https://api.github.com:8443/repos",
			"https://github.com/team/plugins",
		] {
			assert_eq!(auth.for_url(&url(elsewhere)), None, "{elsewhere}");
		}
	}

	/// The built-in catalog's entries come first and win an ID clash; source entries are
	/// tagged with their source, and a catalog cannot tag its own entries.
	#[test]
	fn merged_catalogs_keep_built_in_entries_and_tag_source_ones() {
		let entry = |id: &str| -> extensions::CatalogEntry {
			serde_json::from_value(serde_json::json!({
				"manifest": {
					"api_version": 1, "id": id, "name": id, "version": "1.0.0",
					"author": "a", "license": "MIT", "source": "https://example.org",
					"kind": "plugin", "capabilities": [], "actions": []
				},
				"release_url": "https://example.org/p.serein-extension",
				"sha256": "a".repeat(64),
				"download_bytes": 1,
				"source_commit": "b".repeat(40),
			}))
			.unwrap()
		};
		let catalog = |entries| Catalog {
			api_version: extensions::API_VERSION,
			entries,
		};
		let merged = merge(
			Some(catalog(vec![entry("shared"), entry("official")])),
			vec![(
				"team".into(),
				catalog(vec![entry("shared"), entry("private")]),
			)],
		)
		.unwrap();
		let tags: Vec<_> = merged
			.entries
			.iter()
			.map(|entry| (entry.manifest.id.as_str(), entry.catalog_source.as_deref()))
			.collect();
		assert_eq!(
			tags,
			[
				("shared", None),
				("official", None),
				("private", Some("team"))
			]
		);
		assert!(merge(None, Vec::new()).is_none());
		let mut tagged = serde_json::to_value(entry("x")).unwrap();
		tagged["catalog_source"] = "official".into();
		assert!(
			serde_json::from_value::<extensions::CatalogEntry>(tagged).is_err(),
			"a catalog cannot claim a source"
		);
		let existing = [source("https://example.org/c.json", &[])];
		assert_eq!(new_id("Team Plugins!", &[]), "team-plugins");
		assert_eq!(new_id("team", &existing), "team-2");
		assert_eq!(new_id("???", &[]), "source");
	}
}
