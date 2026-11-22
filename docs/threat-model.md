# Threat model

Community extensions add an untrusted-code boundary. Wasmi runs without WASI or
native handles, with compilation, fuel, stack, linear-memory and output limits. The
only imports are the capability-gated `serein` host functions (preview), each declared
in the manifest and granted by the user, which copy bytes into plugin memory and never
expose host pointers. With `network` (preview) a plugin reaches only the exact HTTPS
origins it declares and the user approved, through public, pinned addresses without
cookies, proxies or Serein credentials; anything the same plugin can read, including
the `message_journal`, can therefore leave the device to those origins, and the consent
dialog says so. Plugin secrets never enter Wasm: the host fills `{{secret.<id>}}`
placeholders only for each secret's declared origins and redacts echoed values from
responses, though a declared origin that is hostile receives the value by design.
Plugins exchange data only through topics the user approved per provider, subscriber and
topic; a subscriber sees only what a provider broadcasts, but data a reading plugin
broadcasts can leave the device through a subscriber with `network`, which the
Connections list warns about. Message hooks run only in channels where the user switched
them on: a send hook reads and may rewrite drafts there, and, with automatic sending
chosen, its rewrite is sent without review; an annotating plugin reads that channel's
messages and can only add notes the host renders as plain text. A notification plugin
sees the sender and preview of every alert Serein would show and may reword or hide it,
but cannot create alerts, silence sounds or delay one past 1.5 s. Plugin pages show only
text the plugin returns, drawn by the host as plain text in bounded rows; they read nothing
from the client themselves. Histories, kept deleted rows and call captions are likewise
plugin-supplied plain text, labelled with the plugin's name and never sent to Discord. A
`files` plugin reaches only its private folder and the folders and files the user picked
for it through the system picker; paths are relative, `..` and symbolic links are refused,
and the plugin never learns full paths. A media worker stays loaded only during a call,
inside the same sandbox with a 64 MiB memory cap and per-batch fuel, and sees only what
the host puts in its batches. With `media_packets` or `media_audio` that includes the
decrypted audio and video of everyone else in the call, which a plugin with `network` or
`files` can send away or keep; the consent dialog says so, and nothing is tapped unless
such a plugin runs. Only explicitly granted, user-selected context crosses that boundary.
The host renders validated descriptions and requires Apply for composer changes;
plugins cannot dispatch arbitrary Discord commands. Optional app snapshots have
separate grants for account/conversation context, channel directory, ordinary loaded
timeline rows, members, presence, voice state, read state and local reading settings.
The host exposes bounded data only, never credentials or raw media. Typed navigation,
clipboard, notice, existing-call and reading-setting proposals require explicit user
confirmation and revalidation in the ordinary host control path. Background handlers
cannot emit proposals. Permission/account changes invalidate copied app context;
voice actions also bind the original call request. Catalog downloads use a separate
credential-free client with bounded bodies, validated/pinned public DNS results,
redirect checks and expected hashes/lengths. Consent binds the reviewed flag and
exact artifact hash, so a cancelled local import cannot substitute for a catalog
release. Catalog review and hashes do not eliminate host/runtime vulnerabilities
or protect against a compromised catalog maintainer. Local extension data is
unencrypted; same-user filesystem tampering is outside the Wasm sandbox.

Protect session credentials, private messages, draft integrity, account isolation and platform permissions. Trust Discord as the service endpoint, but validate transport origins, lengths and response shapes. Server authorization is authoritative.

Credentials are non-serializable, non-Clone, redacted, zeroized in owned Rust buffers where practical, and saved through the OS credential store by default. The explicit Storage opt-in permits bounded unencrypted login files when an OS save fails; see [storage policy](storage-policy.md). Same-user software, administrators and backups can read those files. Temporary copies exist inside HTTP/WebSocket/JS/platform engines; no universal memory-erasure claim is made. A password is never entered into a Rust form. The login bridge is limited to its own newly-created Discord webview and never searches other apps or profiles.

Threats addressed: redirected Authorization leakage, header injection, bot-session substitution, oversized REST/WebSocket input, partial-update corruption, stale session callbacks, uncontrolled queues, automatic ambiguous-write retries, disk account mixing, plaintext-token files without explicit consent and background data collection.

Open risks: unofficial Discord account policy; third-party login page or platform-engine compromise; native dependency supply chain; untested embedded challenge/QR handoff; local users/backups reading unencrypted SQLite; sophisticated permissions/event gaps; incomplete event/subscription coverage; Markdown/font/accessibility coverage; and OS keychain denial/unavailability. No encryption is downgraded for voice; voice is unavailable.

Do not report secrets or raw HTTP/Gateway payloads. Error UI uses fixed categories. Default tests use synthetic markers and local transports. No live network account testing in CI.

Dependency audit policy uses `cargo audit --deny warnings`. ZIP parsing is pinned to
9.0.5 to address RUSTSEC-2026-0367/0368/0369; updater extraction still rejects links
and special files and never invokes bulk archive extraction. Narrow informational
exceptions in `.cargo/audit.toml` cover `ttf-parser` and the standalone `zvariant`,
`zvariant_derive` and `zbus_names` crates. The latter were folded into zbus 6; our
pinned zbus 5 and upstream portal, accessibility, tray and Secret Service dependencies
still require them. These are maintenance warnings, not ignored vulnerabilities.
Reassess these exceptions when the upstream dependency stack migrates to zbus 6.
The audit configuration ignores `RUSTSEC-2026-0387`, which currently affects `crypto-bigint` 0.5.5
(timing variability in operations intended to be constant-time). The affected paths are DAVE
P-256 signing-key generation and MLS key-package validation in `crates/discord-voice`. The DAVE
path arrives via `davey` -> `p256 0.13` -> `elliptic-curve` -> `crypto-bigint`; the MLS path arrives
via `openmls` -> `openmls_rust_crypto` -> `hpke-rs-rust-crypto` -> `p256 0.13` / `p384 0.13`. The upstream
dependencies have not yet migrated to `crypto-bigint >=0.7.6`. The voice subsystem owner maintains
this acceptance until both paths use compatible patched releases. Additional ignores in `.cargo/audit.toml`
cover transitive advisories inherited from upstream (RUSTSEC-2026-0385, RUSTSEC-2026-0381, RUSTSEC-2026-0382,
RUSTSEC-2026-0383, and RUSTSEC-2026-0386). The policy also retains the older
RUSTSEC-2026-0414 exception for `pulldown-cmark`.

The direct `pulldown-cmark` pin is now 0.13.6, addressing RUSTSEC-2026-0433's quadratic
nested-image wikilink parsing. Serein uses the default parser or enables only strikethrough;
it does not enable the affected `ENABLE_WIKILINKS` option. The patched release is used
without adding an advisory exception. See the [upstream fix](https://github.com/pulldown-cmark/pulldown-cmark/pull/1186).

Explicit public attachment hosting adds Catbox as a third-party recipient of one
user-selected file only after per-file consent. Anyone with its returned link can
read the unchanged file and embedded metadata; there is no private-hosting or remote
erasure guarantee. The independent fixed-origin credential-free transport refuses
redirects/retries, bounds streams/responses and validates returned file URLs. Consent
binds the stable selection key, filename/size and original session/conversation;
replaced selections and stale sessions cannot silently upload another file or
insert into another draft.
A concurrent writer restoring identical metadata remains outside observable file
change detection. The dialog offers explicit Copy/Add actions and never sends a
Discord message automatically. Cancellation can leave remote bytes without a link.
