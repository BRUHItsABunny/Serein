//! HTTPS requests behind a plugin's `serein.https_request` import.
//!
//! A plugin granted `network` may reach only the exact origins its manifest declares and the
//! user approved at install. For every hop the host resolves the name itself, refuses the
//! request when any address is private, loopback or otherwise non-public, and pins the
//! connection to the addresses it checked. Redirects are followed only to declared origins;
//! a redirect anywhere else is returned to the plugin unfollowed. No cookies, proxies,
//! automatic decompression or Serein credentials are involved, and the plugin never sees
//! a socket: each call is one bounded request and one bounded response.
//!
//! Secrets the user entered for the plugin appear in requests only as `{{secret.<id>}}`
//! placeholders. The host fills them in for the secret's declared origins, refuses to
//! follow a redirect that would carry one elsewhere, and replaces any echo of a value in
//! the response with `[secret]`, so the plugin never reads one.
//!
//! The call blocks the invocation on the extension worker. It is bounded by a per-request
//! timeout, a wall-clock budget for the whole invocation and a request count, and ends at
//! once when the worker job is cancelled.
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use extensions::Reply;
use extensions::network::{Origin, SecretSpec};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

use crate::store::{Gate, public_ip};

/// Requests one invocation may make.
pub const MAX_REQUESTS: u32 = 32;
/// Wall-clock time all of an invocation's requests may take together.
pub const INVOCATION_BUDGET: Duration = Duration::from_secs(30);
/// Time one request may take unless it asks for less.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest timeout a request may ask for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(15);
/// Request body bytes after decoding.
pub const MAX_REQUEST_BODY: usize = 192 * 1024;
/// Response body bytes read; a larger body fails the request.
pub const MAX_RESPONSE_BODY: usize = 160 * 1024;
const MAX_REQUEST_HEADERS: usize = 32;
const MAX_RESPONSE_HEADERS: usize = 64;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_REDIRECTS: usize = 5;
/// Addresses checked and pinned per name.
const MAX_ADDRESSES: usize = 16;
const METHODS: [&str; 6] = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"];
/// Headers the host controls: framing, routing and connection management.
const RESERVED_HEADERS: [&str; 10] = [
	"host",
	"content-length",
	"transfer-encoding",
	"connection",
	"keep-alive",
	"upgrade",
	"te",
	"trailer",
	"expect",
	"proxy-authorization",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
	#[serde(default = "get")]
	method: String,
	url: String,
	#[serde(default)]
	headers: Vec<(String, String)>,
	#[serde(default)]
	body: Option<String>,
	#[serde(default)]
	body_base64: Option<String>,
	#[serde(default)]
	timeout_ms: Option<u64>,
}

fn get() -> String {
	"GET".into()
}

/// Looks up a secret's value by ID; `None` when the user has not set it.
pub(crate) type Resolve = Box<dyn Fn(&str) -> Result<Option<Zeroizing<String>>, String> + Send>;

/// One invocation's network session for one plugin.
pub struct PluginNetwork {
	origins: Vec<Origin>,
	secrets: Vec<SecretSpec>,
	resolve: Option<Resolve>,
	gate: Gate,
	runtime: Option<tokio::runtime::Runtime>,
	requests: u32,
	started: Option<Instant>,
	/// Test servers listen on loopback without TLS.
	#[cfg(test)]
	loopback: bool,
}

struct Prepared {
	method: wreq::Method,
	url: url::Url,
	headers: wreq::header::HeaderMap,
	body: Vec<u8>,
	timeout: Duration,
	/// Origins every secret in this request may reach; `None` when it carries none.
	secret_origins: Option<Vec<Origin>>,
	/// Values filled in, replaced by `[secret]` wherever the response echoes them.
	redact: Vec<Zeroizing<String>>,
}

impl PluginNetwork {
	/// A session for the origins in `declared`, which the manifest validated.
	pub(crate) fn new(declared: &[String], gate: Gate) -> Self {
		Self {
			origins: declared
				.iter()
				.filter_map(|origin| Origin::parse(origin))
				.collect(),
			secrets: Vec::new(),
			resolve: None,
			gate,
			runtime: None,
			requests: 0,
			started: None,
			#[cfg(test)]
			loopback: false,
		}
	}

	/// Lets requests use the manifest's `secrets`, looked up with `resolve`.
	pub(crate) fn secrets(&mut self, secrets: Vec<SecretSpec>, resolve: Resolve) {
		self.secrets = secrets;
		self.resolve = Some(resolve);
	}

	pub fn request(&mut self, request: &[u8]) -> Reply {
		match self.try_request(request) {
			Ok(bytes) => Reply::Ok(bytes),
			Err(message) => Reply::Failed(message),
		}
	}

	fn try_request(&mut self, request: &[u8]) -> Result<Vec<u8>, String> {
		let prepared = self.prepare(request)?;
		if self.requests >= MAX_REQUESTS {
			return Err(format!("At most {MAX_REQUESTS} requests per invocation"));
		}
		let started = *self.started.get_or_insert_with(Instant::now);
		let remaining = INVOCATION_BUDGET.saturating_sub(started.elapsed());
		if remaining.is_zero() {
			return Err("This invocation's network time is used up".into());
		}
		self.requests += 1;
		self.gate.check()?;
		if self.runtime.is_none() {
			self.runtime = Some(
				tokio::runtime::Builder::new_current_thread()
					.enable_all()
					.build()
					.map_err(|_| "Network is unavailable")?,
			);
		}
		let timeout = prepared.timeout.min(remaining);
		let gate = &self.gate;
		let this = &*self;
		self.runtime.as_ref().expect("built above").block_on(async {
			tokio::select! {
				biased;
				_ = gate.wake_cancel.notified() => Err("Extension operation cancelled".to_string()),
				result = tokio::time::timeout(timeout, this.send(prepared)) => {
					result.map_err(|_| "Request timed out".to_string())?
				}
			}
		})
	}

	/// Parses and checks a request before any network access.
	fn prepare(&self, request: &[u8]) -> Result<Prepared, String> {
		let request: Request = serde_json::from_slice(request).map_err(|_| {
			"Request must be {\"method\", \"url\", \"headers\", \"body\" | \"body_base64\", \"timeout_ms\"} JSON"
				.to_string()
		})?;
		let method = request.method.to_ascii_uppercase();
		if !METHODS.contains(&method.as_str()) {
			return Err("Method must be GET, HEAD, POST, PUT, PATCH or DELETE".into());
		}
		let url = url::Url::parse(&request.url).map_err(|_| "Invalid request URL")?;
		let Some(origin) = self.declared(&url) else {
			return Err("The request URL's origin is not in this plugin's network list".into());
		};
		let mut request = request;
		let (secret_origins, redact) = self.fill_secrets(&mut request, &origin)?;
		let url = url::Url::parse(&request.url).map_err(|_| "Invalid request URL")?;
		if self.declared(&url).as_ref() != Some(&origin) {
			return Err("A secret may not change the request's origin".into());
		}
		if request.headers.len() > MAX_REQUEST_HEADERS
			|| request
				.headers
				.iter()
				.map(|(name, value)| name.len() + value.len())
				.sum::<usize>()
				> MAX_HEADER_BYTES
		{
			return Err("Too many or too large request headers".into());
		}
		let mut headers = wreq::header::HeaderMap::new();
		for (name, value) in &request.headers {
			let name = wreq::header::HeaderName::from_bytes(name.as_bytes())
				.map_err(|_| "Invalid request header name")?;
			if RESERVED_HEADERS.contains(&name.as_str()) || name.as_str().starts_with("proxy-") {
				return Err(format!("The host sets the {name} header"));
			}
			let value = wreq::header::HeaderValue::from_str(value)
				.map_err(|_| "Invalid request header value")?;
			headers.append(name, value);
		}
		if !headers.contains_key(wreq::header::USER_AGENT) {
			headers.insert(
				wreq::header::USER_AGENT,
				wreq::header::HeaderValue::from_static("Serein plugin"),
			);
		}
		if !headers.contains_key(wreq::header::ACCEPT_ENCODING) {
			headers.insert(
				wreq::header::ACCEPT_ENCODING,
				wreq::header::HeaderValue::from_static("identity"),
			);
		}
		let body = match (request.body, request.body_base64) {
			(Some(_), Some(_)) => return Err("Send either body or body_base64".into()),
			(Some(text), None) => text.into_bytes(),
			(None, Some(encoded)) => BASE64
				.decode(encoded)
				.map_err(|_| "body_base64 is not valid base64")?,
			(None, None) => Vec::new(),
		};
		if body.len() > MAX_REQUEST_BODY {
			return Err("Request body exceeds 192 KiB".into());
		}
		if !body.is_empty() && matches!(method.as_str(), "GET" | "HEAD") {
			return Err("GET and HEAD requests have no body".into());
		}
		let timeout = match request.timeout_ms {
			Some(0) => return Err("timeout_ms must be positive".into()),
			Some(ms) => Duration::from_millis(ms).min(MAX_TIMEOUT),
			None => DEFAULT_TIMEOUT,
		};
		Ok(Prepared {
			method: wreq::Method::from_bytes(method.as_bytes()).map_err(|_| "Invalid method")?,
			url,
			headers,
			body,
			timeout,
			secret_origins,
			redact,
		})
	}

	/// Replaces `{{secret.<id>}}` in the URL, header values and text body. Every secret must
	/// be declared, allowed for `origin` and set. Messages never contain a value.
	#[allow(clippy::type_complexity)]
	fn fill_secrets(
		&self,
		request: &mut Request,
		origin: &Origin,
	) -> Result<(Option<Vec<Origin>>, Vec<Zeroizing<String>>), String> {
		let malformed = || "Malformed {{secret.<id>}} placeholder".to_string();
		let mut ids: Vec<String> = Vec::new();
		let texts = std::iter::once(&request.url)
			.chain(request.headers.iter().map(|(_, value)| value))
			.chain(request.body.iter());
		for text in texts {
			for id in extensions::network::placeholders(text).ok_or_else(malformed)? {
				if !ids.iter().any(|known| known == id) {
					ids.push(id.to_owned());
				}
			}
		}
		if ids.is_empty() {
			return Ok((None, Vec::new()));
		}
		let mut allowed = self.origins.clone();
		let mut values = Vec::new();
		for id in &ids {
			let spec = self
				.secrets
				.iter()
				.find(|spec| spec.id == *id)
				.ok_or_else(|| format!("Secret {id} is not declared in the manifest"))?;
			let origins = self.spec_origins(spec);
			if !origins.contains(origin) {
				return Err(format!("Secret {id} may not be sent to {origin}"));
			}
			allowed.retain(|origin| origins.contains(origin));
			let resolve = self
				.resolve
				.as_ref()
				.ok_or("Plugin secrets are unavailable on this device")?;
			let value = resolve(id)?
				.ok_or_else(|| format!("Secret {id} is not set; set it in the plugin's details"))?;
			values.push((format!("{{{{secret.{id}}}}}"), value));
		}
		for (placeholder, value) in &values {
			let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
			request.url = request.url.replace(placeholder.as_str(), &encoded);
			for (_, header) in &mut request.headers {
				*header = header.replace(placeholder.as_str(), value);
			}
			if let Some(body) = &mut request.body {
				*body = body.replace(placeholder.as_str(), value);
			}
		}
		// The URL carries the encoded form, which a response may echo too.
		let mut redact = Vec::new();
		for (_, value) in values {
			let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
			if encoded != *value {
				redact.push(Zeroizing::new(encoded));
			}
			redact.push(value);
		}
		Ok((Some(allowed), redact))
	}

	fn spec_origins(&self, spec: &SecretSpec) -> Vec<Origin> {
		spec.origins
			.iter()
			.filter_map(|origin| {
				#[cfg(test)]
				if self.loopback {
					let url = url::Url::parse(origin).ok()?;
					return Some(Origin {
						host: url.host_str()?.to_owned(),
						port: url.port_or_known_default()?,
					});
				}
				Origin::parse(origin)
			})
			.collect()
	}

	/// The declared origin `url` belongs to, if any.
	fn declared(&self, url: &url::Url) -> Option<Origin> {
		#[cfg(test)]
		if self.loopback && url.scheme() == "http" {
			let origin = Origin {
				host: url.host_str()?.to_owned(),
				port: url.port_or_known_default()?,
			};
			return self.origins.contains(&origin).then_some(origin);
		}
		Origin::of(url).filter(|origin| self.origins.contains(origin))
	}

	async fn resolve(&self, origin: &Origin) -> Result<Vec<SocketAddr>, String> {
		let addresses: Vec<_> = tokio::net::lookup_host((origin.host.as_str(), origin.port))
			.await
			.map_err(|_| format!("Could not resolve {}", origin.host))?
			.take(MAX_ADDRESSES)
			.collect();
		#[cfg(test)]
		if self.loopback {
			return Ok(addresses.into_iter().filter(SocketAddr::is_ipv4).collect());
		}
		if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
			return Err(format!(
				"{} does not resolve to a public address",
				origin.host
			));
		}
		Ok(addresses)
	}

	async fn send(&self, prepared: Prepared) -> Result<Vec<u8>, String> {
		let Prepared {
			mut method,
			mut url,
			headers,
			mut body,
			secret_origins,
			redact,
			..
		} = prepared;
		let mut redirected = false;
		for hop in 0..=MAX_REDIRECTS {
			self.gate.check()?;
			let origin = self
				.declared(&url)
				.ok_or("Redirect left the declared origins")?;
			let addresses = self.resolve(&origin).await?;
			// Pin the checked addresses; every hop gets a freshly checked client.
			let client = wreq::Client::builder()
				.no_proxy()
				.no_gzip()
				.no_brotli()
				.no_deflate()
				.no_zstd()
				.redirect(wreq::redirect::Policy::none())
				.resolve_to_addrs(origin.host.clone(), addresses)
				.build()
				.map_err(|_| "Network is unavailable")?;
			let mut builder = client.request(method.clone(), url.as_str());
			for (name, value) in &headers {
				builder = builder.header(name, value);
			}
			if !body.is_empty() {
				builder = builder.body(body.clone());
			}
			let response = builder
				.send()
				.await
				.map_err(|_| format!("Request to {} failed", origin.host))?;
			let status = response.status();
			if status.is_redirection() && hop < MAX_REDIRECTS {
				let next = response
					.headers()
					.get(wreq::header::LOCATION)
					.and_then(|location| location.to_str().ok())
					.and_then(|location| url.join(location).ok());
				// A redirect to an undeclared origin, or one a secret in this request may not
				// reach, is the plugin's answer, not a new request.
				if let Some(next) = next.filter(|next| {
					self.declared(next).is_some_and(|origin| {
						secret_origins
							.as_ref()
							.is_none_or(|allowed| allowed.contains(&origin))
					})
				}) {
					if matches!(status.as_u16(), 301..=303) && method != wreq::Method::HEAD {
						method = wreq::Method::GET;
						body.clear();
					}
					url = next;
					redirected = true;
					continue;
				}
			}
			return self.reply(response, redirected, &redact).await;
		}
		Err("Too many redirects".into())
	}

	async fn reply(
		&self,
		response: wreq::Response,
		redirected: bool,
		redact: &[Zeroizing<String>],
	) -> Result<Vec<u8>, String> {
		let scrub = |text: String| {
			redact
				.iter()
				.fold(text, |text, value| text.replace(value.as_str(), "[secret]"))
		};
		if response
			.content_length()
			.is_some_and(|length| length > MAX_RESPONSE_BODY as u64)
		{
			return Err("Response body exceeds 160 KiB".into());
		}
		let status = response.status().as_u16();
		let url = response.uri().to_string();
		let mut header_bytes = 0;
		let headers: Vec<(String, String)> = response
			.headers()
			.iter()
			.filter_map(|(name, value)| {
				Some((
					name.as_str().to_owned(),
					scrub(value.to_str().ok()?.to_owned()),
				))
			})
			.take(MAX_RESPONSE_HEADERS)
			.take_while(|(name, value)| {
				header_bytes += name.len() + value.len();
				header_bytes <= MAX_HEADER_BYTES
			})
			.collect();
		let mut bytes = Vec::new();
		let mut stream = std::pin::pin!(response.bytes_stream());
		while let Some(chunk) = futures_util::StreamExt::next(&mut stream)
			.await
			.transpose()
			.map_err(|_| "Response was interrupted")?
		{
			self.gate.check()?;
			if bytes.len() + chunk.len() > MAX_RESPONSE_BODY {
				return Err("Response body exceeds 160 KiB".into());
			}
			bytes.extend_from_slice(&chunk);
		}
		let mut reply = json!({
			"status": status,
			"url": scrub(url),
			"redirected": redirected,
			"headers": headers,
		});
		match String::from_utf8(bytes) {
			Ok(text) => reply["body"] = scrub(text).into(),
			Err(error) => reply["body_base64"] = BASE64.encode(error.into_bytes()).into(),
		}
		Ok(reply.to_string().into_bytes())
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::io::{AsyncReadExt, AsyncWriteExt};

	fn session(origins: &[&str]) -> PluginNetwork {
		PluginNetwork::new(
			&origins
				.iter()
				.map(|origin| origin.to_string())
				.collect::<Vec<_>>(),
			Gate::detached(),
		)
	}

	fn failure(network: &mut PluginNetwork, request: serde_json::Value) -> String {
		match network.request(request.to_string().as_bytes()) {
			Reply::Failed(message) => message,
			other => panic!("expected a failure, got {other:?}"),
		}
	}

	#[test]
	fn requests_are_checked_before_any_network_access() {
		let mut network = session(&["https://api.example.org"]);
		for (request, expected) in [
			(
				json!({ "url": "https://other.example.org/x" }),
				"network list",
			),
			(
				json!({ "url": "https://api.example.org:444/x" }),
				"network list",
			),
			(json!({ "url": "http://api.example.org/x" }), "network list"),
			(
				json!({ "url": "https://user:pw@api.example.org/x" }),
				"network list",
			),
			(
				json!({ "method": "TRACE", "url": "https://api.example.org/" }),
				"Method",
			),
			(
				json!({ "url": "https://api.example.org/", "headers": [["Host", "evil.example"]] }),
				"host",
			),
			(
				json!({ "url": "https://api.example.org/", "headers": [["Proxy-Connection", "x"]] }),
				"proxy-connection",
			),
			(
				json!({ "url": "https://api.example.org/", "body": "x" }),
				"no body",
			),
			(
				json!({ "method": "POST", "url": "https://api.example.org/", "body": "x".repeat(MAX_REQUEST_BODY + 1) }),
				"192 KiB",
			),
			(
				json!({ "url": "https://api.example.org/", "extra": 1 }),
				"JSON",
			),
		] {
			let message = failure(&mut network, request.clone());
			assert!(message.contains(expected), "{request}: {message}");
		}
		// Nothing above counted as a request.
		assert_eq!(network.requests, 0);
	}

	/// Serves the responses `build` returns for the server's port, one connection each, in
	/// order; the task returns the raw requests.
	async fn serve(
		build: impl FnOnce(u16) -> Vec<String>,
	) -> (u16, tokio::task::JoinHandle<Vec<String>>) {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		let responses = build(port);
		let task = tokio::spawn(async move {
			let mut seen = Vec::new();
			for response in responses {
				let (mut socket, _) = listener.accept().await.unwrap();
				let mut request = Vec::new();
				let mut buffer = vec![0; 64 * 1024];
				// Read the head, then as much body as its Content-Length announces.
				loop {
					let read = socket.read(&mut buffer).await.unwrap();
					request.extend_from_slice(&buffer[..read]);
					let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
					let Some(head) = text.find("\r\n\r\n") else {
						if read == 0 {
							break;
						}
						continue;
					};
					let length = text[..head]
						.lines()
						.find_map(|line| line.strip_prefix("content-length:"))
						.and_then(|value| value.trim().parse::<usize>().ok())
						.unwrap_or(0);
					if read == 0 || request.len() >= head + 4 + length {
						break;
					}
				}
				seen.push(String::from_utf8_lossy(&request).into_owned());
				socket.write_all(response.as_bytes()).await.unwrap();
				socket.shutdown().await.unwrap();
			}
			seen
		});
		(port, task)
	}

	fn loopback(port: u16) -> PluginNetwork {
		let mut network = PluginNetwork::new(&[], Gate::detached());
		network.origins = vec![Origin {
			host: "localhost".into(),
			port,
		}];
		network.loopback = true;
		network
	}

	#[test]
	fn responses_follow_only_declared_redirects_and_stay_bounded() {
		let server = tokio::runtime::Runtime::new().unwrap();
		let (port, task) = server.block_on(serve(|port| {
			let large = "x".repeat(MAX_RESPONSE_BODY + 1);
			vec![
				format!(
					"HTTP/1.1 307 Temporary Redirect\r\nLocation: http://localhost:{port}/next\r\nContent-Length: 0\r\n\r\n"
				),
				"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok".into(),
				"HTTP/1.1 302 Found\r\nLocation: https://elsewhere.example.org/\r\nContent-Length: 0\r\n\r\n"
					.into(),
				format!(
					"HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{large}",
					large.len()
				),
			]
		}));
		let mut network = loopback(port);
		let url = format!("http://localhost:{port}/start");
		// A declared redirect keeps the method and body (307) and is followed.
		let reply = network.request(
			json!({ "method": "POST", "url": url, "body": "payload", "headers": [["X-Key", "1"]] })
				.to_string()
				.as_bytes(),
		);
		let Reply::Ok(bytes) = reply else {
			panic!("request failed: {reply:?}")
		};
		let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(reply["status"], 200);
		assert_eq!(reply["body"], "ok");
		assert_eq!(reply["redirected"], true);
		// A redirect elsewhere comes back unfollowed.
		let Reply::Ok(bytes) = network.request(json!({ "url": url }).to_string().as_bytes()) else {
			panic!("request failed")
		};
		let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(reply["status"], 302);
		assert_eq!(reply["redirected"], false);
		// Oversized bodies fail instead of truncating.
		assert!(failure(&mut network, json!({ "url": url })).contains("160 KiB"));
		let seen = server.block_on(task).unwrap();
		assert!(
			seen[0].starts_with("POST /start")
				&& seen[0].contains("x-key: 1")
				&& seen[0].ends_with("payload")
		);
		assert!(seen[1].starts_with("POST /next") && seen[1].ends_with("payload"));
		assert!(
			seen[0]
				.to_ascii_lowercase()
				.contains("user-agent: serein plugin")
		);
		assert!(!seen[0].to_ascii_lowercase().contains("cookie"));
		// The request budget is per invocation.
		network.requests = MAX_REQUESTS;
		assert!(failure(&mut network, json!({ "url": url })).contains("At most"));
	}

	fn with_secret(
		mut network: PluginNetwork,
		origins: &[&str],
		value: Option<&str>,
	) -> PluginNetwork {
		let value = value.map(str::to_owned);
		network.secrets(
			vec![SecretSpec {
				id: "api_key".into(),
				label: "API key".into(),
				origins: origins.iter().map(|o| o.to_string()).collect(),
			}],
			Box::new(move |id| {
				Ok(value
					.clone()
					.filter(|_| id == "api_key")
					.map(Zeroizing::new))
			}),
		);
		network
	}

	#[test]
	fn secrets_fill_only_declared_set_and_allowed_placeholders() {
		let both = ["https://api.example.org", "https://other.example.org"];
		let mut network = with_secret(
			session(&both),
			&["https://api.example.org"],
			Some("s3cr3t-value"),
		);
		for (request, expected) in [
			(
				json!({ "url": "https://api.example.org/?k={{secret.missing}}" }),
				"not declared",
			),
			(
				json!({ "url": "https://other.example.org/", "headers": [["Authorization", "{{secret.api_key}}"]] }),
				"may not be sent",
			),
			(
				json!({ "url": "https://api.example.org/", "headers": [["X", "{{secret.api_key"]] }),
				"Malformed",
			),
			(
				json!({ "url": "https://{{secret.api_key}}.example.org/" }),
				"network list",
			),
		] {
			let message = failure(&mut network, request.clone());
			assert!(message.contains(expected), "{request}: {message}");
			assert!(!message.contains("s3cr3t"), "{message}");
		}
		let mut unset = with_secret(session(&both), &["https://api.example.org"], None);
		let message = failure(
			&mut unset,
			json!({ "url": "https://api.example.org/?k={{secret.api_key}}" }),
		);
		assert!(message.contains("not set"), "{message}");
		let mut undeclared = session(&both);
		let message = failure(
			&mut undeclared,
			json!({ "url": "https://api.example.org/?k={{secret.api_key}}" }),
		);
		assert!(message.contains("not declared"), "{message}");
		assert_eq!(network.requests, 0);
	}

	#[test]
	fn filled_secrets_reach_their_origin_and_never_come_back() {
		let server = tokio::runtime::Runtime::new().unwrap();
		let (port, task) = server.block_on(serve(|_| {
			vec![
				"HTTP/1.1 200 OK\r\nX-Echo: key s3cr3t value\r\nContent-Length: 22\r\n\r\nyou sent s3cr3t value!".into(),
				// The next hop is declared, but this request's secret may not go there.
				"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n".into(),
			]
		}));
		let mut network = loopback(port);
		network.origins.push(Origin {
			host: "127.0.0.1".into(),
			port: 1,
		});
		let origin = format!("http://localhost:{port}");
		let mut network = with_secret(network, &[origin.as_str()], Some("s3cr3t value"));
		let url = format!("http://localhost:{port}/q?key={{{{secret.api_key}}}}");
		let request =
			json!({ "url": url, "headers": [["Authorization", "Bearer {{secret.api_key}}"]] });
		let Reply::Ok(bytes) = network.request(request.to_string().as_bytes()) else {
			panic!("request failed")
		};
		let text = String::from_utf8(bytes).unwrap();
		assert!(!text.contains("s3cr3t"), "{text}");
		let reply: serde_json::Value = serde_json::from_str(&text).unwrap();
		assert_eq!(reply["body"], "you sent [secret]!");
		let Reply::Ok(bytes) = network.request(request.to_string().as_bytes()) else {
			panic!("request failed")
		};
		let reply: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
		assert_eq!(reply["status"], 302);
		let seen = server.block_on(task).unwrap();
		assert!(
			seen[0].starts_with("GET /q?key=s3cr3t+value "),
			"{}",
			seen[0]
		);
		assert!(seen[0].contains("authorization: Bearer s3cr3t value"));
	}

	#[test]
	fn private_addresses_are_refused_even_for_declared_names() {
		let mut network = session(&["https://localtest.me"]);
		network.origins = vec![Origin {
			host: "localhost".into(),
			port: 443,
		}];
		let message = failure(&mut network, json!({ "url": "https://localhost/" }));
		assert!(message.contains("public address"), "{message}");
	}
}
