# Community extensions

Serein extensions are local, opt-in tools for the native client. The Extensions
and Themes pages in Settings contain packages, links to their source, their
requested capabilities and their review status. A plugin cannot directly call Discord, read credentials, open files or make
network requests. Separately granted foreground actions can propose messages
and other app operations; the user reviews and applies each operation in Serein.

## Install and remove

Themes and plugins are published together from [`extensions/`](../extensions/README.md)
in this repository. Opening Settings > Themes or Extensions checks its shared catalog
on the existing worker. Normal builds embed no package payloads; bundled examples
remain available only as demo/test fixtures. Demo catalog refresh, previews and
selected catalog installs use the same public repository downloads as normal builds.
An identical bundled fixture with the approved ID and hash is reused without a download.
Catalog packages, plugin source and previews have one canonical copy in `extensions/`;
tests and demos read it directly. Runtime catalog refresh reads the catalog published
from `main`.
The last valid catalog and installed packages remain available offline. Refresh
retries immediately. Catalog changes add/remove available choices and mark
installed updates; they never install, update or delete packages automatically.
A refresh shows a spinner without disabling local controls; choosing a local action
cancels a pending catalog/thumbnail request.
Updating an inactive theme preserves the current selection. Locally created or
imported packages are never replaced by catalog updates.

Enable downloads the selected, hash-pinned package after its capabilities have
been accepted. Updates are manual and require renewed capability consent.
Import selects a local `.serein-extension` JSON package. An import is unreviewed;
importing alone does not grant it capabilities or execute it.

Disable stops accepting results immediately, then removes Serein's downloaded
package, temporary files and extension data. A failure to remove files is shown
and cleanup is retried on the next load. Re-enabling requires downloading or
importing the package again and starts with fresh extension settings. Serein
never deletes the creator's Git repository, the user's imported original, or an
exported theme/image source.

Plugin grants and data belong to the signed-in account, except the narrow
`api_proxy` connection plugin described below. Logout invalidates
plugin results, drains bounded in-flight work and clears that account's extension data. Theme selection is a device
preference. There is no periodic background polling or automatic package update.

## Catalog sources (preview)

Settings > Extensions > **Catalog sources** adds up to 8 catalogs beside the built-in one,
such as a private plugin repository. A source has a name, a catalog address (public
HTTPS, the same format as `extensions/catalog.json`), an optional token, optional extra
origins the token may go to, and whether it refreshes with the built-in catalog; one that
does not refreshes only from its **Refresh** button.

- **Tokens** are saved in the system credential store, never in `sources.json`. A token is
  sent as `Authorization: Bearer` only to the catalog's own origin and the listed origins,
  checked on every redirect hop, so a redirect elsewhere, such as GitHub's signed release
  storage, goes without it. Changing a source's address or token origins forgets its
  token unless a new one is entered. Preview images are always fetched without a token.
- **Packages** keep the catalog's pinned SHA-256 and size, and downloads keep the public
  address and redirect rules of the built-in catalog.
- **Entries** from a source show "from <source>" on their card. An ID the built-in catalog
  already lists is skipped, and a catalog cannot claim a source for its entries. Installed
  packages update from the catalog they came from, like built-in ones.
- **GitHub**: requests to `api.github.com` ask for raw file contents (`/contents/`) and
  release assets (`/releases/assets/`). A repository can publish with
  [`extensions/templates/publish-plugin.yml`](../extensions/templates/publish-plugin.yml),
  then be added with `https://api.github.com/repos/OWNER/REPO/contents/catalog.json` and,
  if private, a fine-grained token with read access to its contents.

## Creator workflow

1. Keep source and license in a public Git repository. Use the standalone Rust
   [Message Delete Protector source](../extensions/plugins/message-delete-protector/src/lib.rs) and the
   [SDK](../extensions/sdk).
   The [SDK authoring guide](../examples/extensions/README.md#test-and-develop-locally)
   covers native handler tests, typed panel values and JSON storage helpers; the v1
   exports and existing plugin source remain compatible. For reactive plugins,
   use [Message Counter](../examples/extensions/message-counter/src/lib.rs) and the
   [message-event guide](../examples/extensions/README.md#reactive-message-plugins).
   [App Toolbox](../examples/extensions/app-toolbox/src/lib.rs) demonstrates app
   snapshots and user-confirmed host actions.
2. Build a Wasm module implementing the version 1 ABI documented by the starter.
   No native binary, installer, Git hook or build script runs on an end user's
   computer. Other languages can implement the same Wasm buffer/JSON contract.
3. Package the manifest and Wasm bytes (or declarative theme) as a single JSON
   file. Test through Import with synthetic `--demo` conversations first; catalog browsing
   may download public GitHub content.
4. Add the package and reproducible source/build instructions under `extensions/`
   in a pull request; CI validates it. After merge, a workflow pins `catalog.json`
   to the merged commit, recording immutable package URLs, byte lengths and SHA-256
   hashes. See the [extensions README](../extensions/README.md) for exact commands.
5. Maintainers review each listed version, its capabilities and the source to
   artifact relationship. A catalog checksum identifies reviewed bytes; it is
   not a signature or a guarantee that code is harmless. Updates need review too.

The in-app catalog reads `extensions/catalog.json` from this repository's `main`
branch. A new entry is listed only after it is merged and the catalog is pinned. Empty catalogs
are valid; imports allow development before a release is listed.

## Shop previews

Catalog entries may include a short `description` (at most 256 characters and
1,024 UTF-8 bytes, without control characters) and a `preview` object:

```json
{
  "preview": {
    "url": "https://example.org/releases/v1/preview.png",
    "sha256": "<64 hexadecimal SHA-256 digits>",
    "download_bytes": 12345
  }
}
```

Use an original or licensed PNG/JPEG screenshot showing the theme or plugin in
use. Pin its URL to an immutable release or source commit, then record the exact
file size and digest. Prefer a 16:9 image; the shop preserves its aspect ratio.
Offline demo theme fixtures show palette thumbnails using their actual colors. Clicking a demo
or installed theme thumbnail previews its full appearance in the normal app, with
Back to themes and Customize actions. Demo themes can be customized without installing
them first; saving creates an editable local copy and preserves the original package.
Remote themes download only when installed; installed themes can be previewed or
customized through the existing flow. Normal builds embed no extension or theme packages.
Editor-created themes may embed a separate local card cover; it replaces the palette
illustration and is center-cropped to 16:9. It is not a catalog preview URL and does
not change the conversation background.
The protector shows a deleted-row illustration. Other entries without an image
remain valid and show a built-in illustration.
Previews describe the listed version, including when an installed version has
an update available; they are creator-provided, not proof of compatibility.

Only visible shop cards request previews. Images use the credential-free,
public-IP-pinned HTTPS downloader with a five-second deadline and must match their
own hash and byte count. Preview loading does not block shop actions; an action
cancels preview-only work before starting.
Decoding runs on the existing cancellable worker: at most 256 KiB compressed,
4,096 pixels per edge, 4,194,304 source pixels, and 32 MiB decoder allocation
budget. Only a static image is decoded; thumbnails shrink to at most 640 x 360.
Missing, invalid or unavailable images fall back without blocking installation.
The UI retains at most eight thumbnails (at most 7,372,800 RGBA bytes), clears
changed-image metadata and releases them with its extension runtime state.
There is no preview disk cache, telemetry, new dependency, or plugin permission.

## Host contract

The `extensions` crate defines the versioned manifest, capability, action,
invocation, result and theme types. These serialized types are the compatibility
boundary; internal `client-core` structures and egui objects are not an SDK.
Unknown API versions and invalid packages are rejected before installation.

### Capability reference

Each capability is independent and requires user consent. An update requests
renewed consent; adding a read grant does not grant commands. The SDK currently
supports 73 capabilities, with at most 96 distinct declarations per manifest.
`external_navigation` and `style.icon_size` are Preview APIs from PR #937, not yet released.

> **Preview SDK — PR #411, not yet released.** `channel_control`,
> `server_control`, `role_control`, `moderation_control` and `media_control`, plus
> the reply, sticker and forward operations under `message_send`, require a host
> built from this branch.

| Capability | Granted behavior | Scope / confirmation |
| --- | --- | --- |
| `selected_message` | Read selected message text | A user-invoked `message` action only |
| `composer` | Read the current draft and propose replacement | A `composer` action; replacement requires Apply |
| `storage` | Read/replace one opaque local UTF-8 value | Per-account/plugin; 1 MiB disk limit and 256 KiB invocation budget |
| `api_proxy` | Configure the Discord REST API HTTP/HTTPS proxy | Preview; device-wide, available before login; only `storage` may accompany this grant; no account data, Gateway, CDN or calls |
| `rich_presence` | Contribute one bounded custom activity and restore it during activation | Preview; explicit panel/activation actions only; separate activity-sharing preference; revoked on disable/account change |
| `deleted_messages` | Enable host retention of already-loaded deleted messages | Activation only; bounded session memory, no deleted text sent to Wasm |
| `image_sharing` | Legacy compatibility capability | Activation output still validates, but emoji/sticker image fallback is now built in and cannot be controlled by a plugin |
| `appearance` | Return a bounded declarative theme overlay | Native colors/control metrics; no arbitrary drawing |
| `message_events` | Observe live create/update/delete events | Active accessible conversation; bounded best-effort delivery |
| `app_context` | Read connection, current user and selected channel | Current session, optional fields |
| `account_profile` | Read the current account's loaded avatar hash and own profile | Connected; optional display name, bio and pronouns; no credentials/connections |
| `guild_directory` | Read loaded joined servers | At most 100 names/IDs/icon hashes; no fetch |
| `channel_details` | Read selected-channel metadata, recipients and permission summary | Fresh accessible channel; at most 32 recipients; last-message ID/count require history access |
| `data_events` | Opt into fifteen granted app-data invalidation reasons | Requires `app_events` plus each reason's read grant |
| `channel_directory` | Read accessible cached channels with their parent and position | At most 100, plus up to 250 from the focus server; partial directory, no ordering promised, no fetch |
| `message_content` | Read loaded embed text, sticker labels, reply/forward markers and poll availability | Fresh readable selected timeline; 10 messages/8 KiB; no media URLs/bytes or poll details |
| `forum_data` | Read resident accessible child threads/posts and known flags | Selected readable parent/thread; 10 posts/6 KiB; no archive fetch or tags |
| `conversation_activity` | Read current typing IDs and a loaded pin page | Selected readable conversation; 8 typing IDs/20 pin IDs/2 KiB; no fetch |
| `channel_metadata` | Read loaded guild channel topic/category/thread metadata and permission decisions | Fresh readable selected guild channel; unknown remains optional; 6 KiB; no settings fetch |
| `member_details` | Read loaded guild member nicknames, role labels and matching server profile | Fresh selected member pane; 20 members/32 role IDs each/32 catalog roles; 6 KiB; no fetch |
| `message_details` | Read loaded message metadata: replies, mentions, attachment labels and reaction counts | Fresh readable selected timeline; at most 20 messages (12 with `timeline`); no text or URLs |
| `relationships` | Read loaded friends, requests and restricted-account labels | Connected; at most 100; known flags distinguish unloaded lists; no fetch |
| `timeline` | Read ordinary loaded messages in the active conversation | Fresh readable timeline, at most 50 (12 with `message_details`); no deleted/ephemeral text |
| `members` | Read loaded members or DM recipients | Active channel, at most 100; no fetch |
| `presence` | Read cached status strings for that context | At most 100; no activity/private device payloads |
| `voice_state` | Read call state, the 64-ID participant list of the current call, and up to 128 roster entries with names, mute, deafen and speaking flags across voice channels the account can view | Roster rows carry bounded, sanitized names; the roster keeps its own 16-KiB byte budget within the snapshot; no raw media |
| `read_state` | Read current channel unread/mention summary | Unknown unread remains distinct from false |
| `local_settings` | Read and propose changing seven local reading preferences | Zoom/sidebar/member list/GIFs/media links/smooth scrolling/scroll speed; changes require Apply |
| `notification_settings` | Read and propose changing device-local notification preferences | Sound volume, sound toggles and unread badge; changes require Apply; no Discord notification settings |
| `navigation` | Propose channel, Home, view, profile, message or search navigation | Existing native paths; each command requires Apply |
| `external_navigation` | Propose opening one reviewed absolute HTTPS URL in the browser | Preview; full 2,048-byte maximum URL shown and revalidated at Apply; native link-confirmation setting applies; no plugin networking |
| `local_notices` | Propose an in-app toast | At most 1,024 text bytes; requires Apply |
| `clipboard_write` | Propose replacing clipboard text | At most 4,096 bytes; requires Apply; no clipboard read |
| `voice_control` | Propose mute/deafen, leaving or watching/stopping a participant stream | Each command requires Apply and the same current call |
| `app_events` | Observe ready/navigation/context/connection/voice/settings changes | Other grants control accompanying data; no background commands |
| `context_menus` | Add actions to user, channel, server and role context menus | The invocation names the clicked item by ID (a role adds its name and color); results follow the action's other grants |
| `interface_tweaks` | Switch on host-defined interface tweaks such as still typing dots, plain profiles, always-expanded roles, hidden request counts, one alert per direct conversation until read, hidden messages from blocked users, hidden profile notes, a larger stream preview or double-click to reply or edit | Activation and panel results only; a tweak stays on while any enabled plugin requests it and switches off when that plugin is disabled, fails or the account logs out |
| `interface_ui` | Replace the client's main panes with a bounded declarative row tree (indent, generic icon, tint, badge, emphasis, selected) whose rows may also carry a native voice join; row clicks invoke the plugin's declared `interface_click` panel action (the one reserved action id allowed to contain `_`) with the row id and a double-click flag, which Enter on a focused row or dock item also sets as the primary open action (Space is a single click), and `window` shows one native window beside the tree: the active call's stage or an accessible channel's conversation view; `dock` adds small server tiles at the left of the tree pane's bottom control row (the loaded server's icon via `guild_id`, or the label's initials when unknown, with hover labels, a selected ring and row-like click routing), and `add_server` appends the client's own native add-a-server button after them; `controls` picks the native buttons at the right of that row, left to right, from `disconnect` (only during a call), `mute`, `deafen` and `settings`, and the host always appends `settings` when missing so the plugin can still be turned off; a `native` join row shows the loaded voice channel's real name and, with `voice_connect`, its double click or Enter joins through the client's own join path (current mute and deafen, call-switch confirmation) with no Apply step, since the double-clicked host-labelled row is the confirmation (an unknown, non-voice or hidden channel leaves the row inert with the plugin's text); `size` asks for a window size while the layout shows | Activation, panel and event results restate the tree; `None` restores the normal layout; at most 512 rows, 256-byte labels, 32-byte badges, unique 64-byte row ids (unique across rows and dock items), indent at most 8, at most 64 dock items with 1-256-byte labels and snowflake `guild_id`s, no repeated controls, snowflake native channel ids, `size` 200-1600 by 200-1200 logical pixels (centered and clamped to the monitor, the previous placement restored afterwards, never saved); the host renders the window, dock icons, the add-server button, the controls and native join labels itself, so no message content reaches Wasm; other row-click effects still wait for Apply |
| `start_screen` | Replace the start screen's logo and copy with the plugin's own pre-rendered images and plain text | Preview; activation and panel results only; one bounded device-level file, shown on the connecting screen before any account loads and without running Wasm; the host draws nothing of its own design: up to four 2-bit palette cell images, each still or looping at up to 30 frames per second, stack top to bottom, then an optional title and subtitle in the normal font, scaled down only to fit (see the [image format](extension-sdk-actions.md#start-screen-preview)); an optional `window` size of 240-1,600 × 240-1,200 logical pixels is clamped to the monitor and centered while the screen shows, never saved as the window size, and the previous size, position and maximized state return afterwards; when several plugins set one, the lowest plugin id wins; disabling, removing or failing the plugin, revoking its grant, ending its account session or returning `start_screen: null` restores the default; the progress bar and **Use a different account** always stay |
| `clock` | Import `serein.clock_now_ms` to read the current Unix time in milliseconds | Preview; [host import](#host-imports-preview); no account data |
| `random` | Import `serein.random_fill` for operating-system secure random bytes | Preview; [host import](#host-imports-preview); at most 4 KiB per call |
| `database` | Import `serein.db_execute` for a private SQLite database | Preview; [plugin database](#plugin-database-preview); per-account/plugin; 64 MiB by default, up to 16 GiB by user choice |
| `network` | Make HTTPS requests to the exact origins in the manifest's `network` list | Preview; [network](#network-preview); the consent dialog lists the origins and warns that readable data may be sent there |
| `message_journal` | Receive every message create, edit and delete the account sees, with earlier text where the host holds it, through one `journal` action | Preview; [message journal](#message-journal-preview); all conversations, including DMs; at-least-once |
| `send_hook` | Rewrite the user's draft in one `send` action before it is sent | Preview; [message hooks](#message-hooks-preview); only in channels where the user switches it on, with preview or automatic sending and a chosen fallback |
| `message_annotations` | Add notes under messages from one `annotate` action, and under the message a `message` action was chosen on | Preview; [message hooks](#message-hooks-preview); `annotate` batches come only from channels where the user switches it on; notes are shown, never sent |
| `notifications` | See each desktop notification, with its message preview, in one `notification` action and reword or hide it | Preview; [notification hook](#notification-hook-preview); the original shows when the plugin fails or is too slow |
| `pages` | Show a page in the main area from activation and panel actions, with panel elements above a long list the host fetches a screenful at a time | Preview; [plugin pages](#plugin-pages-preview); rows are drawn by the host, never sent to Discord |
| `call_captions` | Show up to four caption lines above the call controls during a call | Preview; [call captions](#call-captions-preview); from any action except hooks and page row fetches, including `topic` deliveries |
| `files` | Read and write files in a private folder, and in folders and files the user picks for it, with `serein.file_request` and `serein.file_write` | Preview; [files](#files-preview); the private folder counts against the data limit; picks are listed in the plugin's details and can be removed |
| `media_worker` | Keep one instance loaded during calls that runs one `media` action per batch of call events and, with media taps, call media | Preview; [media worker](#media-worker-preview); only during a call; may return only `captions` |
| `media_packets` | Receive the decrypted Opus, H.264, H.265 and AV1 packets of everyone else in the call in `media` batches, and ask senders for keyframes | Preview; [media taps](#media-taps-preview); requires `media_worker`; the user's own microphone and camera are not included |
| `media_audio` | Receive each other speaker's audio as 16 kHz mono 16-bit samples in `media` batches | Preview; [media taps](#media-taps-preview); requires `media_worker` |
| `media_archive` | Save original message attachments from Discord's CDN into the private folder with `serein.attachment_fetch` | Preview; [attachment archive](#attachment-archive-preview); requires `files`; counts against the data limit |
| `message_send` | Propose sending text, replies, loaded stickers, a loaded-message forward, or opening the native attachment picker | Apply; selected accessible targets; picker discloses no file path/bytes to Wasm; preserves existing composition |
| `message_manage` | Propose editing, deleting or pinning a loaded message | Apply; native ownership/moderation checks |
| `reactions_control` | Propose adding/removing your reaction | Apply; known message and current reaction state |
| `read_state_control` | Propose channel/server read markers | Apply; existing read-state checks |
| `threads_control` | Propose creating, renaming or changing threads/forum posts | Apply; native channel/thread permissions |
| `relationship_control` | Propose friend requests, relationship changes, nicknames and user notes; open a friend DM | Apply; existing relationship and loaded-note checks |
| `account_control` | Read own status/activity preferences and propose profile, status and activity-sharing changes | Apply; profile readiness and native validation |
| `audio_settings` | Read audio preferences and propose processing, gain, local playback, device selection or a device refresh | Apply; selected IDs must remain in the host's private device lists |
| `voice_connect` | Propose joining/ringing or declining a call | Apply; connection/access checks; native call-switch confirmation |
| `camera_control` | Propose enabling/disabling your camera or selecting a host-known camera | Apply; same current call, capture availability and native permissions |
| `channel_control` | Propose channel creation/editing/deletion/reordering, channel notification settings, and DM/group controls | Apply; loaded targets and native permissions; destructive operations are identified before approval |
| `server_control` | Propose server-settings/emoji changes, creating an invite or leaving a server | Apply; loaded server/settings and native permission checks |
| `role_control` | Propose creating, editing, moving or deleting roles | Apply; native role hierarchy and permission checks |
| `moderation_control` | Propose role assignment, nicknames, kicks, prune preview/execution and member-list visibility | Apply; native hierarchy/permission checks; destructive actions are identified |
| `media_control` | Propose opening the native screen-share picker or stopping screen share | Apply; current-call checks; no source list or captured media reaches Wasm |

### Custom Rich Presence

The preview [Custom Rich Presence plugin](../extensions/plugins/custom-rpc)
provides a native editor with validated fields and a local preview. It covers
activity type, application ID/name, details/state and links, large/small artwork,
two buttons, party size and timers. **Preview changes** is local; **Apply presence**
saves the configuration and contributes it through the host's presence pipeline.
Sharing must be enabled separately in Serein; the plugin does not turn it on.

**Stop presence** removes the contribution and disables saved automatic resume
while retaining the last applied fields. **Disable** removes the package and its
saved settings under the ordinary extension lifecycle. Account changes and logout
also revoke pending/current contributions. When several plugins contribute an
activity, the first in ascending plugin-ID order wins, within the existing limit
of eight installed plugins. Custom activity takes precedence over detected game
activity, which can resume when it is cleared.
Custom configurations and resolved activities each have a 3-KiB serialized limit.
The complete Gateway presence payload has a 4-KiB cap; when adding Spotify would
exceed it, the host omits Spotify for that update and preserves custom activity
and account status.

Creators use `RichPresenceOutput` and the `rich_presence` capability; `storage`
remains a separate grant. The [typed contract](extension-sdk-actions.md#custom-rich-presence)
documents Set/Clear, bounds, timers and `Element::ActivityPreview`. Existing ABI v1
plugins and original SDK output literals stay compatible. Unsupported older hosts
reject the new capability. This is an unofficial service path: synthetic preview
and sandbox checks are not proof of cross-client Discord behavior.

### App snapshots and confirmed commands

The [app data reference](extension-sdk-reference.md#app-data) and
[output/action reference](extension-sdk-actions.md#outputs-and-host-actions)
explain every field, command and bound, with examples. SDK authors use
`fn(AppInvocation) -> AppOutput` with the unchanged `export!` macro and
`dispatch_typed` for offline checks. These wrappers retain the original
`Invocation`, `EventInvocation` and `Output` APIs and flatten into ABI v1 JSON.
Unsupported capabilities/actions are rejected by older hosts; declaring v1 alone
does not make new capabilities available in an old build. Current hosts also
inject public `host` discovery into every invocation: API version 1, SDK revision
1 and supported capability/event names. `AppInvocation.host` is optional for
older hosts, and its SDK lists use strings to tolerate future names. Support is
not consent; discovery never bypasses required manifest validation or grants.

`app` contains separately granted optional `context`, `account_profile`, `guilds`,
`channel_details`, `channels`, `timeline`, `members`, `presence`, `voice`,
`read_state`, `settings`, `notification_settings`, `message_details`, `relationships`, `channel_metadata`,
`member_details`, `message_content`, `forum_data`, `conversation_activity`,
`audio_settings` and `own_presence` groups. A missing group
is unavailable or ungranted, not an empty dataset. Snapshot construction reads
already-loaded state without network or disk IO. The complete serialized snapshot
is capped at 64 KiB. Per-group budgets are 10 KiB for channels, 20 KiB for timeline,
6 KiB each for members, presence and channel-detail recipients, and 8 KiB for guilds,
including item overhead. The voice roster keeps its own 16-KiB budget (at most 128
entries with names and flags, beside the 64-ID call participant list). The channel
directory first lists every channel of a focus server on its own budget of up to
250 channels and 48 KiB, then fills the shared
100-channel, 10-KiB budget with the rest; directory items are dropped from the end if
the snapshot would otherwise pass 64 KiB. The focus server is the `guild_id` of the
dock item an `interface_click` names, otherwise that of the selected dock item in the
plugin's last restated interface, otherwise the natively selected server. Directory
items carry `parent_id` and `position` so plugins can order them; the host promises
no order beyond the focus server first. Message details and relationships use at most 8 KiB and
4 KiB respectively, and share the remaining 64-KiB snapshot budget; they may
truncate earlier when other groups are present. Channel metadata and member
details each have a 6-KiB ceiling within that same global budget; member rows
are trimmed first, and groups can be omitted when no space remains. Rich-message
content (8 KiB), forum data (6 KiB) and conversation activity (2 KiB) also share
that ceiling; no new total snapshot allocation is authorized. The timeline
skips messages larger than 4 KiB and reports partial data. Granting
`message_details` also caps both timeline text and metadata rows at 12 when requested together, while preserving
the 20-KiB timeline byte budget and shared 10-million-fuel limit. Valid wire-sized
inputs can still exceed execution fuel. Bounded list responses
expose `truncated`; voice participant IDs are capped without a completeness flag.
No snapshot includes tokens, deleted/ephemeral text, attachment bytes/URLs, raw
voice/video/screen media or the unbounded client state.
An active DM or private channel is eligible when accessible: granting timeline,
members or presence can expose that conversation's corresponding loaded data.

Foreground `message`, `composer` and `panel` actions may return one `effects`
proposal, at most 8 KiB serialized. The native result shows its exact action and
values and waits for **Apply**. Closing it does nothing. Apply rechecks the plugin,
grants, account/conversation, permissions and the specific voice call before using
the existing UI path. Navigation/search may then load ordinary service data, but
the plugin never receives a generic Discord command API. Clipboard writes never
read the clipboard; local notices are in-app toasts, not OS notifications. External-navigation
proposals show and revalidate one absolute HTTPS URL before handing it to the native browser flow;
they do not expose an HTTP client or DNS/public-IP pinning because the OS browser owns that request.

Supported local preference patches are zoom 50–150%, sidebar width 190–360 logical
pixels, member-list visibility, GIF animation, hiding media links, smooth scrolling
and scroll speed 25 through 300%. The separate `notification_settings` grant exposes
device-local notification toggles and sound volume 0 through 100%; it does not change
Discord account/guild settings or play a cue. Both patches require at least one
value, preserve omitted/null preferences at Apply time and reject invalid changes
without applying any fields. Changes use the ordinary local persistence path. The separate `voice_connect` and `camera_control` grants permit approved call
joins/rings and camera changes. Screen-source selection and recording are not
exposed. Read the [app-action reference](extension-sdk-actions.md#app-actions)
for messaging, threads, relationships, account and audio operations.

Four opt-in additions extend the same ABI without changing existing SDK structs:

- `data_queries` adds `ExtendedAppInvocation.queries` and approved request actions for
  message search, pins, archived threads, member search, profiles and GIF search.
  Results reuse Serein's bounded native views: at most 25 rows and 48 KiB total,
  with loading, error, truncation and next-cursor fields where applicable.
- `messaging_settings` adds the loaded account privacy snapshot and approved updates
  for DM, message-request, friend-source and game-DM preferences.
- `guild_folders` adds the loaded server-folder layout and an approved whole-layout
  update through the existing versioned, 1,000-folder/1,000-server native path. Updates
  carry the snapshot's `base_version` and reject stale full-layout replacements.
- `action_feedback` adds `tracked_app_action`. Supply a unique `request_id`; after
  **Apply**, the app-event handler receives `action_result` with `accepted` or
  `rejected` and a stable code (`accepted`, `context_changed`, `unavailable`,
  `invalid`, `denied` or `failed`). Acceptance means Serein admitted the action to
  its native path; it does not claim that a later network request succeeded.

Use `ExtendedAppInvocation` only when reading these fields. Existing
`AppInvocation` source remains compatible and ignores the additional JSON fields.
`data_queries` and `action_feedback` require one `app_event` action because query
completion and tracked results use that bounded, best-effort queue. Query
invalidations may be coalesced; always read the latest snapshot.

Approved convenience actions also open the native join-server flow, send a server
invite to a loaded friend, open emoji/member/role/invite/audit-log administration,
and open a group-DM editor. The native UI still performs its ordinary access and
permission checks. Integrations, webhooks and slash commands remain outside this
SDK surface.

One `app_event` action may observe `ready`, `navigation`, `context`, `connection`,
`voice` and `settings`. Settings events cover reading, notification, audio and own-presence/activity
preferences; each snapshot still requires its own grant. Context
events report loaded-data/freshness changes after navigation; use `message_events` for individual message changes. The additional
`data_events` grant opts into `account`, `channels`, `members`, `presence`,
`read_state`, `message_details`, `relationships`, `threads`, `roles`, `permissions`,
`recovered`, `reactions`, `pins`, `typing` and `polls` invalidation hints (21 event kinds total), each requiring the corresponding data grant
(`channels` accepts `channel_directory`, `guild_directory`, `channel_details` or `channel_metadata`; `members`
accepts `members` or `member_details`). `threads` requires `channel_metadata` or `forum_data`,
`roles` requires `member_details`, and `permissions`/`recovered` accept any of
`channel_metadata`, `member_details`, `forum_data`, `conversation_activity` or
`message_content`. `reactions`/`pins`/`typing` require `conversation_activity`,
`polls` requires `message_content`, and `message_details` accepts either its
namesake grant or `message_content`.
Recovery is an invalidation hint, not event replay or a promise all data is fresh. Without this
opt-in, existing observers receive only the original six variants. Repeated
pending detailed reasons coalesce by kind for each plugin. Pending app
changes are coalesced per plugin and share the message-event queue/rate bounds.
The host captures a fresh, separately granted snapshot at dispatch. There are no
timers or persistent plugin instances, and delivery is best effort. App/message
events and activation cannot return host command proposals; background events
cannot open panels. Granted `storage` and `appearance` outputs remain available.

The [App Toolbox example](../examples/extensions/app-toolbox/src/lib.rs) provides
an account/channel dashboard and explicit controls for the original 12 host-effect types.
[Conversation Actions](../examples/extensions/app-actions/src/lib.rs) demonstrates
the new typed `app_action` operations through a review form. Its `app_event`
observer returns an empty output, and it saves no conversation data.
[Guild Inspector](../examples/extensions/guild-inspector/src/lib.rs) separately
shows optional channel settings/thread permissions and the first five loaded
member details, with only its two read grants plus event grants. It does not
fetch settings, profiles or role catalogs. Unknown decisions remain distinct
from denial, and native actions still recheck permissions.
[Conversation Inspector](../examples/extensions/conversation-inspector/src/lib.rs)
shows rich summaries, loaded forum flags, typing/pin state and public discovery.
Polls have only an absent/unsupported marker; no questions/results are retained.
Forum tags remain unavailable. All three inspector/toolbox observers stay passive.

### Existing tools and message events

Actions are invoked by a message context-menu item, composer tool or panel
button. A plugin may also declare one `activation` action that runs in the worker
on enable/account load. An enabled plugin with granted `deleted_messages` capability
keeps already loaded deleted messages in the session window. Without that opt-in,
deleted payloads are released. Deleted text is red by default. Hover and a
local context menu can toggle that highlight or remove the retained row. They
never call Discord. Live service actions stay unavailable. Tombstone
reconciliation and disk-cache removal remain unchanged. Disabling the plugin, logout, lost channel
access and ordinary timeline eviction also clear retained deleted content. The
same 500-row / 4 MiB per-window budget includes both live and retained deleted
payloads. The optional `preserve_deleted_messages` activation output is accepted
for package compatibility; successful activation with the granted capability enables
retention, including existing protector packages with no-op activation. Input is
restricted to the granted context and bounded form values.
Results can propose a composer replacement or return native headings, text, rows,
separators, buttons, text inputs, checkboxes, dropdowns and integer sliders. Standalone
panel actions are available from Settings > Extensions. Composer actions appear in the composer Tools menu. Composer proposals require Apply, retain
the ordinary Send action and are discarded when their originating context is
stale. Account/session changes invalidate outstanding results.

The `message_events` capability allows one `message_event` action to receive
read-only create, update and delete snapshots for the active, accessible
conversation. The user must explicitly grant this access. Only accepted live
timeline events are eligible; history, search, cache loads, ephemeral messages
and other conversations are excluded. Create supplies channel/message/author IDs
and text; updates may contain partial fields; delete supplies channel and message
IDs only. Text is bounded to 16 KiB of UTF-8. The payload excludes attachments,
embeds and raw Gateway data. It can contain private conversation text when that
conversation is active, so grant access only to plugins you trust.

Event handlers run on the existing worker and may save `storage` or change
`appearance` with those separately granted capabilities. They cannot send messages,
replace the composer, open panels in the background or enable activation-only
features. Use a separate user-invoked panel to show results. The Message Counter
example stores only three saturating numeric counts, preserves malformed storage
until the user explicitly resets it, and never stores message text or identifiers.

Delivery is best effort, not exactly once or a complete audit stream. The host
requires a loaded message for updates/deletes; duplicate creates, including sends
already reconciled from a send result, may be skipped. The host
queues at most 32 pending invocations totaling 64 KiB and starts at most 10 event
invocations per second; overload drops events. Scope/account changes, lost
permissions and disabling discard pending work and reject stale results.
Reactive SDK handlers use `EventInvocation` and `dispatch_typed`; existing
`Invocation` handlers and their struct literals remain unchanged under ABI v1.
Older hosts reject the new capability/action in the manifest. They do not load
a message-event plugin merely because its manifest declares API version 1.

Emoji/sticker image fallback is built into the client and needs no extension.
Emoji and stickers usable in the current conversation keep their native Discord
send path. The account's session-reported Nitro entitlement controls animated and
external custom emoji eligibility and external server sticker eligibility.
Picking fallback artwork inserts an inline composer preview and never submits it.
Explicit Send uses `[name](public-artwork-url)` Markdown when the draft also contains
text. Serein renders these named links as inline artwork; other clients choose their
own link/preview presentation. A draft containing only artwork sends up to ten ordinary
image attachments after host validation, within an 8 MiB prepared batch limit.
Animated APNG stickers become GIF attachments. A newer text draft typed during
preparation is preserved. Existing file selections use the normal message path.
Artwork attachments display at 48px for emoji and 160px for stickers. Sending rows
have no Restore to composer action; failed/unknown sends keep their existing controls.
Normal attachment permissions and account upload limits still apply. Logout,
account changes and channel navigation cancel pending image preparation.
The retired Emoji & Sticker Images plugin is no longer offered. Its `image_sharing`
capability and output remain accepted under the existing activation/grant validation
for ABI compatibility, but have no effect on this built-in behavior.

Themes override named colors and native typography, spacing, padding and corner
radii, plus an optional embedded PNG/JPEG background with per-mode opacity and fit.
Settings > Themes > Create theme provides a native editor and portable export.
See the complete [theme API](theme-api.md) for fields, bounds and inheritance.

Plugins can request the `appearance` capability to return an `appearance` object
using that same theme schema. Creators can build native appearance settings panels
with dropdowns and sliders. Activation and separately granted event actions
can also return appearance values.
An action replaces that plugin's previous appearance object; omit it to leave the
current appearance unchanged, or return `{}` to remove its overrides. A plugin can
use granted `storage` to save choices and read them during activation on the next
account load. Activation does not receive conversation text. The host overlays
plugin appearances on the selected theme in ascending plugin-ID order; the last
explicit value wins. The user's own accent color setting still takes precedence.
Disabling a plugin removes its overrides; Ctrl+Shift+F12 resets community themes
and appearance plugins.

Preview `tick` actions can update that same appearance overlay on a host-controlled,
completion-paced schedule. They run only while signed in and enabled, only when the
shared worker is idle, and pause after failure until disable/re-enable. The host eases
hex colors between results; plugins receive elapsed `tick_ms`, not a render callback.

Appearance supports app-wide palette changes and shared native control styling, not
arbitrary code injection into egui, custom fonts, network access, or automatic Discord
actions. Layout replacement is limited to the separate `interface_ui` capability's
declarative row tree. Fixed-size custom-painted components keep their existing geometry.
Missing fields inherit the underlying theme.

## Safe execution diagnostics

Execution errors use fixed host messages and existing native error status. They
never include plugin output, message text, saved storage, raw Wasm error strings
or private invocation values. No diagnostic log or upload is added. Categories
identify a useful next check, not a complete crash trace:

| Host error category | Author action |
| --- | --- |
| `Fuel` | Reduce handler loops, JSON work or requested data; the limit is 10 million fuel. |
| `Memory` | Reduce memory/table allocation; linear memory remains capped at 16 MiB. This category also covers engine allocation failure, not only hitting that exact cap. |
| `Stack` | Reduce recursion and stack allocations. |
| `Trap` | Check for panic, invalid memory access or arithmetic traps. No panic text is exposed. |
| `Input` / `InputLimit` | Check the action/input schema or reduce data, form values and storage. Public discovery counts toward the existing 256-KiB serialized invocation limit. |
| `Handler` | The ABI returned no response. Check SDK input decoding and output serialization; the host cannot infer which failed from an empty response. |
| `Output` / `OutputLimit` | Check output JSON/ABI buffer or reduce panel elements, text and storage. Existing output limits still apply. |
| `Capability` | Check the manifest, user grants and permitted action surface; an execution retry cannot grant access. |
| `Module` / `Execution` | Check required exports, import restrictions and runtime requirements. |
| `Invalid` / `Version` / `Limit` | Check document schema, supported API version or the relevant package/data bounds before execution. |

Native SDK `dispatch_typed` checks remain useful for decoding and serialization;
real sandbox checks exercise fuel, memory, exports and output enforcement. Neither
proves live Discord compatibility. No lifecycle tests are implied by these diagnostics.

## Resource and privacy limits

Wasm executes on an on-demand background worker with fuel, stack and memory
limits, no WASI, and only the capability-gated [host imports](#host-imports-preview).
Each invocation gets a new runtime; it is
dropped when the call completes. Plugin execution never happens in a render or
audio callback. The application bounds package sizes, installed plugin count,
invocation input/output, panel complexity, queues and plugin storage.

| Resource | Limit | What it means for an author |
| --- | --- | --- |
| Compiled Wasm | 4 MiB | Keep the module small; the host validates imports and compilation limits too. |
| JSON package | 16 MiB | Includes the manifest and encoded payload. |
| Linear memory | 16 MiB | Includes decoding, handler allocations and output buffers. |
| Execution fuel | 10,000,000 | Shared by parsing and execution; a valid-sized input can still exhaust it. |
| Wasm call depth / interpreter stack | 128 calls / 256 KiB | Avoid deep recursion. |
| Serialized input and output | 256 KiB each | Count UTF-8 and JSON escaping, including nested storage JSON. |
| Manifest actions / capabilities | 16 / 96 distinct | Only the 73 supported capability names are currently accepted. |
| Network | 16 declared origins; 32 requests and 30 s per invocation; 15 s, 192 KiB sent and 160 KiB received per request | See [network](#network-preview). |
| Host calls | 4,096 per invocation; 1,000 fuel each plus 4 per byte copied | Exceeding either ends the call with the fuel error. |
| Panel | 64 elements / 8 row levels | Includes nested children; text and input values are at most 4 KiB each. |
| Plugin storage on disk | 1 MiB | Its practical size must also fit the smaller invocation/output budget. |
| Plugin database | 64 MiB file by default (user-selectable 64 MiB-16 GiB); 256 KiB per request and reply; about 100 million SQLite instructions per invocation | See [plugin database](#plugin-database-preview). |
| App snapshot | 64 KiB | Individual lists have smaller budgets; see the [data reference](extension-sdk-reference.md#app-data). |
| Host proposals | 1 / 8 KiB serialized | A foreground action proposes one operation for Apply. |
| Reactive queue / rate | 32 pending calls / 64 KiB / 10 starts per second | Shared message/app events are best effort; excess work is dropped. |

Invalid output or exhausted Wasm budgets produce an execution error and can
disable the failing plugin. Snapshot collectors may instead return explicitly
partial lists, and reactive overload may drop events; handle both in your code.

Disabled plugins have no retained instance, worker or package once cleanup succeeds;
their data is kept or deleted as the user chose (see [plugin data](#plugin-data-preview)).
 Shared host code and bounded catalog metadata still cost some
application space. Freed allocations may remain in the process allocator; an
unchanged RSS reading alone does not mean an instance remains active.

Plugin storage and plugin databases are ordinary local data, not encrypted by Serein. Do not use it
for credentials. No extension diagnostics or private invocation data are
uploaded. Sandboxing and review reduce exposure but cannot prove absence of
bugs in the runtime or host; keep Serein updated.

The buffer ABI is documented in the
[authoring guide](../examples/extensions/README.md#abi-version-1). The demo uses
a separate bounded temporary `serein-extension-demo` profile; it can import local
fixtures and browse the embedded starter catalog/previews but cannot download a
catalog, preview or package. `Ctrl+Shift+F12` resets a
community theme if its colors make controls difficult to read.

## Host imports (preview)

A plugin module may import functions from the Wasm module `serein`; nothing else
can be imported. Each function needs its capability declared in the manifest and
granted by the user:

| Import | Signature | Capability | Result |
| --- | --- | --- | --- |
| `clock_now_ms` | `() -> i64` | `clock` | Unix time in milliseconds, or `-1` when unavailable |
| `random_fill` | `(ptr: i32, len: i32) -> i32` | `random` | Writes `len` (0-4,096) secure random bytes at `ptr`; `0` success, `-1` invalid length, `-2` unavailable |
| `db_execute` | `(ptr: i32, len: i32) -> i32` | `database` | Runs one JSON request of at most 256 KiB; the pending reply's length, `-3` with a pending error message of at most 1 KiB, `-1` invalid length, `-2` unavailable |
| `journal_read` | `(max: i32) -> i32` | `message_journal` | Reads 1-256 entries after the plugin's journal position; the pending reply's length, `-3` with a pending error, `-1` invalid count, `-2` unavailable |
| `https_request` | `(ptr: i32, len: i32) -> i32` | `network` | Performs one JSON request to a declared origin; the pending response's length, `-3` with a pending error, `-1` invalid length, `-2` unavailable |
| `broadcast` | `(ptr: i32, len: i32) -> i32` | manifest `broadcasts` | Sends `{"topic", "payload"}` (payload at most 16 KiB); the number of approved subscribers reached, or `-1` invalid / `-2` limit |
| `has_subscribers` | `(ptr: i32, len: i32) -> i32` | manifest `broadcasts` | `1` when the topic has an approved subscriber, else `0` |
| `topic_read` | `(max: i32) -> i32` | manifest `subscribes` | Reads 1-64 delivered messages in a `topic` action; the pending reply's length |
| `file_request` | `(ptr, len) -> i32` | `files` | Runs one JSON file request of at most 4 KiB; the pending reply's length (raw bytes for `read`, JSON otherwise) |
| `file_write` | `(ptr, len, data, data_len) -> i32` | `files` | Writes up to 1 MiB of raw bytes as a JSON header says; `0`, or `-3` with a message pending |
| `media_read` | `(ptr, offset, len) -> i32` | `media_worker` | Copies bytes of the current media batch's data; the copied length, or `-1` outside the data |
| `attachment_fetch` | `(ptr, len) -> i32` | `media_archive` | Queues `{"url", "size", "path"}`; `0` once queued, or `-3` with a message pending |
| `result_read` | `(ptr: i32, cap: i32) -> i32` | none | Copies the pending reply when it fits in `cap` bytes and clears it; its length, or `-1` when nothing is pending or `cap` is too small |

Installation rejects a module that imports an unknown name, uses a different
signature or module name, or imports a function whose capability the manifest
does not declare. An invocation fails with the capability error before any Wasm
runs when an imported function's capability is not granted. Host functions copy
bytes into the module's own memory and never expose host pointers; a write past
linear memory traps. Calls count toward the invocation's fuel and its host-call
budget (see [limits](#resource-and-privacy-limits)). Rust authors use
`serein_extension_sdk::host`; only functions a plugin calls become imports.
`HostInfo.host_imports` lists the names a host provides.

Calls with a variable-size reply return its length and keep it pending; a later
`result_read` copies it, and the next such call discards any unread reply. The
reply is charged as copied bytes when it is produced.

## Plugin database (preview)

A plugin granted `database` has one SQLite file, `data.sqlite`, in its
[data directory](#plugin-data-preview). Like `storage`, it is per account and per
plugin and outlives updates, disabling, logout and reinstalling unless the user deletes
it. Rust authors call `serein_extension_sdk::host::db::execute(sql, params)`.

Each `db_execute` request is JSON, `{"sql": "...", "params": [...]}`, with one
statement and `?` parameters. Values are `null`, booleans (stored as integers),
numbers, strings or `{"blob": "<base64>"}`. A reply is
`{"columns": [...], "rows": [[...]], "changes": n, "last_insert_rowid": n}`,
with the same value encoding; `changes` and `last_insert_rowid` are `0` for
read-only statements.

All requests in one invocation share a transaction. Serein commits it only after
the invocation finishes and its output is accepted, so an error, trap, exhausted
fuel, cancelled job or rejected output leaves the database unchanged. Plugins
cannot run `BEGIN`, `COMMIT` or `ROLLBACK`, but `SAVEPOINT`, `RELEASE` and
`ROLLBACK TO` work inside it.

| Rule | Limit |
| --- | --- |
| File size | 64 MiB unless the user picks 256 MiB, 1 GiB, 4 GiB or 16 GiB in the plugin's details; a write past it fails and discards the invocation's database work. Lowering the limit below the current size keeps existing data but refuses growth. |
| Request / reply | 256 KiB each; a larger result fails instead of truncating |
| SQLite work | About 100 million virtual-machine instructions per invocation. Exhausting it fails that statement and every later one, and the invocation's database work is discarded. |
| Statement | 100,000 bytes of SQL, 256 parameters, 256 columns, expression depth 100, values up to 1 MiB |
| Refused | `ATTACH`, `DETACH`, `VACUUM`, transaction statements, temporary tables, views and triggers, virtual tables, `load_extension`, writes to `sqlite_schema` |
| Pragmas | Only `user_version`, `table_info`, `table_xinfo`, `table_list`, `index_list`, `index_info`, `index_xinfo`, `foreign_key_list`, `foreign_key_check`, `quick_check` and `integrity_check` |

Foreign keys are enforced. Serein's bundled SQLite has no full-text search or
R-tree modules. SQL errors are returned to the plugin as text; they never contain
host paths.

## Message journal (preview)

A plugin granted `message_journal` declares one action with `surface: "journal"`.
Serein then records every message create, edit and delete the account receives from
the gateway, in every conversation it can read including DMs, to a journal shared by
the account's journal plugins. The host invokes the `journal` action whenever entries
are waiting; the action reads them with `serein.journal_read`
(`serein_extension_sdk::host::journal::read`) and may return only `storage`.

Delivery is at least once. Each plugin has a position in the journal that moves only
when the invocation succeeds and its output is accepted, immediately after the
plugin's database commits. A failed, cancelled or rejected invocation sees the same
entries again, so entries should be stored idempotently (by `seq` or message ID).

| Entry `kind` | Fields | Meaning |
| --- | --- | --- |
| `create` | `channel_id`, `guild_id`, `message` | A new message |
| `update` | `channel_id`, `guild_id`, `message_id`, changed `content` / `edited_at_ms` / `attachments`, `previous` | An edit. Embed refreshes and reactions are not journaled. |
| `delete` | `channel_id`, `guild_id`, `message_id`, `bulk`, `previous` | A deletion, one entry per ID of a bulk delete |
| `gap` | `reason`: `overflow` with `dropped`, or `pruned` with `from_seq` / `to_seq` | Entries were lost: the host fell behind, or the journal's bounds removed entries before this plugin read them |
| `session_started` | none | A new gateway session began; anything while disconnected or closed is not journaled |
| `missed` | `channel_id`, `after_message_id`, `latest_message_id` | At a new session, a channel's latest message is newer than the last one journaled |
| `channel_changed` | `channel_id` | A channel was created or its details changed |
| `access_lost` | `guild_id` (`null` for DMs), `channel_ids` (at most 1,000 per entry) | These channels were readable before an event and are not after it: a kick or leave, a deleted channel, a permission change, or a server becoming unavailable (an outage looks the same) |
| `access_changed` | none | Which conversations the account can read may have changed |

Every entry also has `seq` (increasing, never reused) and `at_ms` (when the host
recorded it). Newer hosts may add kinds; ignore unknown ones.

A `message` holds `id`, `author` (`id`, `name`, `nick`, `bot`, `webhook`), `content`,
`created_at_ms`, `edited_at_ms`, `type`, `flags`, `reply_to`, `forwarded`, embed and
sticker counts, and `attachments` (`id`, `filename`, `content_type`, `size`, `url`,
`proxy_url`). An entry over 64 KiB drops attachment details and sets `truncated`.

`previous` is the version Serein held in memory: the open conversation's loaded
messages. For other conversations it is `null`; a plugin that stores each `create`
already has the earlier text. History pages, search results and messages sent before
the plugin was enabled are not journaled.

Bounds: the journal keeps at most 100,000 unread entries or 64 MiB, pruning the oldest
into a `pruned` gap; up to 1,024 records wait for the writer before an `overflow` gap.
Entries every consumer has read are deleted. Nothing is recorded while no enabled
plugin consumes the journal, and a plugin enabled later starts with new entries.
Disabling a plugin removes its position. The journal lives with the account's plugin
data, so logging out keeps it; with no consumer installed nothing new is recorded.

## Network (preview)

A plugin granted `network` lists the HTTPS origins it may reach in its manifest:

```json
"capabilities": ["network"],
"network": ["https://api.example.org", "https://upload.example.org:8443"]
```

Origins are exact: `https`, a DNS name with at least one dot, an optional port and
nothing else. No wildcards, paths, credentials, IP literals or `localhost` names; at
most 16, no duplicates, and the capability and the list must come together. The consent
dialog lists the origins and warns that anything the plugin can read may be sent there.

Each `serein.https_request` (`serein_extension_sdk::host::https::send`) is one request,
`{"method", "url", "headers": [[name, value]], "body" | "body_base64", "timeout_ms"}`,
and one complete response, `{"status", "url", "redirected", "headers", "body" |
"body_base64"}`. Text bodies arrive as `body`, others base64-encoded.

- Methods: GET, HEAD, POST, PUT, PATCH, DELETE. GET and HEAD carry no body.
- The host sets `Host`, `Content-Length`, framing and connection headers; `Proxy-*` is
  refused. `User-Agent` defaults to `Serein plugin`, and `Accept-Encoding` to `identity`.
  Bodies are never decompressed.
- Each hop's name is resolved by the host; any non-public address (private, loopback,
  link-local, carrier-grade NAT, documentation or multicast ranges) refuses the request,
  and the connection is pinned to the checked addresses.
- Redirects to declared origins are followed (up to five, each re-checked; 301-303 become
  GET). A redirect elsewhere is returned as the 3xx response, unfollowed.
- No cookies, system proxy, Discord session, token or Serein fingerprint is used.
- Limits: 32 requests and 30 seconds of network time per invocation; per request
  10 seconds by default and 15 at most, 192 KiB sent, 160 KiB received (a larger
  response fails rather than truncating), 32 request and 64 response headers.
- The invocation waits on the extension worker while a request runs; cancelling the
  job, such as on navigation or logout, ends it at once.

## Plugin secrets (preview)

A plugin with `network` may declare secrets the user enters, such as API keys:

```json
"secrets": [
  { "id": "api_key", "label": "DeepL API key", "origins": ["https://api-free.deepl.com"] }
]
```

IDs are 1-32 lowercase letters, digits and `_`; labels at most 64 characters; at most 8
secrets; every origin must also be in `network`. The consent dialog names them, and
the plugin's details show each one with its origins, whether it is set, and a masked
field to enter, replace or remove it. Values (1-4,096 characters without control
characters) are saved only in the OS credential store; the demo keeps them in memory.
Without an OS credential store, secrets are unavailable rather than written to a file.

The plugin never reads a value. It writes `{{secret.<id>}}` in a request's URL, header
values or text body, and the host fills it in (URL-encoded in the URL) when:

- the secret is declared, set, and the request's origin is one of its origins;
- otherwise the request fails before any network access, naming the secret but never
  its value.

A redirect is not followed to an origin any of the request's secrets may not reach, and
every occurrence of a value (raw or URL-encoded) in the response's URL, headers and text
body is replaced by `[secret]`. Values pass through host memory and the HTTP client
only for the request; Serein zeroizes its own copies where practical.

Secrets follow the plugin's data: keeping data on disable or logout keeps them; deleting
the plugin's data also removes them from the credential store.

## Broadcasts between plugins (preview)

A provider lists versioned topics, such as `serein.captions/1`, in `broadcasts`; a
subscriber lists them in `subscribes` and declares exactly one action with
`surface: "topic"`, which may return only `storage`. Shared payload formats are defined in
[extension topics](extension-topics.md).

```json
"broadcasts": ["serein.captions/1"]
```

- **Approval.** Nothing flows until the user turns on that provider, subscriber and topic
  in Settings > Extensions > Connections, which lists every pairing the enabled plugins
  allow and warns when the subscriber can send data to network origins. Approvals are
  saved per account (at most 64) and end when either plugin is disabled.
- **Delivery.** `serein.broadcast` copies the payload to each approved subscriber's
  queue; with none it is dropped before copying (`has_subscribers` lets a provider skip
  the work). The host then invokes the subscriber's `topic` action, which reads with
  `serein.topic_read`. Messages leave the queue only when that invocation is accepted,
  so a failed delivery is retried with the next one (after a 10-second pause).
- **Bounds.** Payloads are at most 16 KiB; 32 broadcasts per invocation; each subscriber
  queue holds 64 messages / 256 KiB. A full queue records a `gap` item with the provider,
  topic and number dropped instead of losing messages silently.
- **No history.** Queues live in memory only; nothing is replayed to subscribers that
  connect later, and logout, an account switch or a revoked connection discards them.
  A plugin that needs history stores what it receives in its own database.

## Message hooks (preview)

Two actions let a plugin take part in conversations the user picks. Both are switched on
per channel from the composer's **Extensions** menu, under **In this channel**; nothing
runs anywhere else, and disabling the plugin switches them off everywhere. The settings
are saved per account (at most 512 channels).

**Send hook.** With `send_hook`, one action with `surface: "send"` receives the draft in
`composer` and its destination in `send_target` (`channel_id`, `guild_id`) when the user
sends. It returns the text to send in `replacement`, or nothing to send the draft
unchanged; it may also save `storage`, nothing else. The user chooses per channel:

- **Show me the rewrite first** (default): the rewrite replaces the draft, a line above
  the composer names the plugin, and the next Enter sends it, edited or not. **Use my
  original** puts the original back and sends it unhooked.
- **Send the rewrite automatically**: the rewrite is sent at once.
- **If it fails**: keep the message unsent (default) or send it unchanged. A failure,
  an empty or oversized rewrite, or a plugin that stops running counts as failing; the
  plugin keeps running for other messages.

A draft edited while the hook runs is neither replaced nor sent. One plugin per channel
holds the send hook; attachments are sent with the rewritten text, and attachment-only
messages skip the hook. Only one draft waits for a hook at a time.

**Annotations.** With `message_annotations`, one action with `surface: "annotate"`
receives `annotate`: `channel_id`, `guild_id` and up to 25 `messages` (`message_id`,
`author_id`, `content`, `edited`), oldest first, at most 128 KiB of text. The host sends
the open channel's latest loaded messages when it opens, then new and edited ones, each
message once per text. The action returns `annotations`: `{"message_id", "text"}` for
messages in its batch, at most 4 KiB each; empty text removes the plugin's note. A
`message` action of a plugin with this grant may annotate the message it was chosen on
the same way. Notes appear in small muted text under the message with the plugin's name,
are kept in memory only (2,000 messages, 2 MiB) and are never sent to Discord. Up to
four plugins may annotate one channel.

```json
{"annotations": [{"message_id": "1300000000000000000", "text": "Hi everyone"}]}
```

**History and deleted rows.** The same results may return `histories` and
`deleted_messages`, under the same rules: each names a message the action was given, once.

- `histories: [{"message_id", "revisions": [{"time_ms", "text"}]}]` gives up to 8 earlier
  versions of a message, oldest first. The message's **(edited)** label then opens them,
  labelled with the plugin's name; a message the client does not know as edited gets a
  **(history)** label. An empty list removes the plugin's history for it.
- `deleted_messages: [{"after_message_id", "messages": [{"message_id", "author",
  "time_ms", "text"}]}]` shows deleted messages the plugin kept as struck-through rows
  under the message they followed, marked "deleted · kept by" the plugin. At most 25 rows
  per result; a deleted row cannot be one of the given messages. An empty list removes
  the plugin's rows there.

Texts follow page rows: at most 4 KiB, line breaks allowed. Like notes, they live in RAM
within the same 2,000-message, 2 MiB budget and never reach Discord.

## Call captions (preview)

With `call_captions`, any action except `send`, `annotate`, `notification` and page row
fetches may return `captions`: up to 4 lines of at most 512 bytes, without control
characters or line breaks, each with an optional `speaker_id` and `final`. They replace
the plugin's previous lines; an empty list clears them. The host shows them in a bar
above the call controls, with the speaker's name from the call roster (or the plugin's
name), and unfinished lines dimmer. Lines fade 10 seconds after the plugin's last update,
are shown only during a call, and at most two plugins' lines show at once. A `topic`
subscriber, such as one receiving `serein.captions/1`, can return them from each delivery.

```json
{"captions": [{"speaker_id": "1200000000000000000", "text": "Can everyone hear me?", "final": true}]}
```

## Notification hook (preview)

With `notifications`, one action with `surface: "notification"` sees each desktop alert
before the operating system shows it. It runs only after Serein's own checks (mutes, Do
Not Disturb, the focused channel and your notification settings) picked the alert, so it
cannot add alerts. The action receives `notification`:

| Field | Meaning |
| --- | --- |
| `source` | `gateway` (the live connection) or `push` (a mobile push message; desktop sends only `gateway`) |
| `message_id`, `channel_id`, `guild_id`, `author_id` | The message; `guild_id` is absent in direct and group messages |
| `direct` | From a direct or group conversation |
| `title`, `body` | What the alert would show: sender name and preview, at most 256 and 512 bytes |
| `upgrade` | The alert was already shown, and a reworded one replaces it |

It returns `notification`: `{"hide": true}` to skip the alert, or `title` and/or `body` to
reword it, within the same limits and without control characters or line breaks. Leaving
it out shows the alert unchanged. It may also save `storage`, nothing else. Hiding affects
only the alert; message sounds and unread badges stay as they are.

- **Order.** Several enabled plugins run one after another in the order they are listed;
  each sees the previous one's wording. A plugin that hides the alert ends the chain.
- **Fails open.** A plugin that fails is skipped. An alert still waiting after 1.5 s is
  shown as it stands; a plugin answering within 10 s after that replaces it with the
  reworded one, unless a newer alert was shown since or the alert was read. At most 16
  alerts wait at a time; more are shown without plugins.
- **One alert per message.** The host remembers the last 512 alerted messages. A message
  reported by both sources alerts once; when a `push` alert is followed by its `gateway`
  copy, the gateway copy replaces it.

```json
{"notification": {"body": "Hello, how are you?"}}
```

## Plugin pages (preview)

With `pages`, an activation or panel action may return `page` to show a page in the main
area: a `title` (at most 128 bytes), panel `elements` and an optional `list`. A later
result with `page` replaces it, `"page": null` closes it, and leaving it out keeps it.
The page also closes when the user closes it, opens another conversation, or the plugin
stops. Buttons run the panel action they name with the page's input values, as in a
result window.

The `list` names its content `version` (1–64 ASCII bytes), its `total` rows (at most
1,000,000), a declared panel action `rows_action`, optionally a declared `click_action`
(run with the row's id in `values["row"]`), up to 100 inline `rows` and an `empty_text`.
The host draws only the rows on screen and asks for missing ones, 50 at a time and one
request at a time, by running `rows_action` with `page_request {version, start, count}`.
That action answers with `page_rows {version, start, rows}`: the same version and start
and at most `count` rows. A fetch may also return `page` and `storage`, and nothing that
would open a window. The host keeps at most 2,000 fetched rows (8 MiB) in RAM, dropping
those farthest from view, and forgets them when the version changes.

A row has an `id` (at most 64 bytes), an optional `author` and `time_ms`, `text` (at most
4 KiB; line breaks allowed, other control characters not), an optional `state` of
`edited` or `deleted`, and up to 8 earlier `revisions` (`time_ms`, `text`), oldest first.
Rows show one line with the full text on hover; deleted rows are struck through, and a row
with revisions has an **(edited) · history** button that opens its versions.

```json
{"page_rows": {"version": "v3", "start": 50, "rows": [
  {"id": "1300000000000000000", "author": "Ada", "time_ms": 1763700000000,
   "text": "See you at 6", "state": "edited",
   "revisions": [{"time_ms": 1763699000000, "text": "See you at 5"}]}]}}
```

## Files (preview)

With `files`, a plugin reads and writes files through two imports. Every call names a
`root` and a relative `path` (`/`-separated, at most 1 KiB):

- `"private"` is the plugin's own folder, `files/` in its data directory. It counts
  against the plugin's data limit (the size chosen in its details, 64 MiB by default),
  is kept and removed with the rest of its data, and needs no pick.
- Any other root is the id of a folder or file the user picked for the plugin. An
  activation or panel action asks with `pick`: `kind` (`folder`, `file` or `save_file`),
  `write`, a `purpose` (at most 128 bytes, shown in the picker title) and, for
  `save_file`, a suggested `name`. The host shows the system picker and then runs the
  declared panel `action` with the new root in `values["root"]`, or with
  `values["cancelled"] = "true"`. A `save_file` pick may always be written; other picks
  only with `write`. A plugin keeps at most 16 picks, saved with its data settings, and
  the user removes them from **Folders and files you gave it** in its details.

`file_request` takes JSON tagged by `op`: `handles` (the picks, with each one's own name
but never its full path), `list` (`offset`, `limit` up to 512, sorted by name, links left
out), `stat`, `read` (`offset`, `length` up to 256 KiB, raw bytes back),
`create_folder`, `delete` (a file or an empty folder) and `rename` (`to`, within the same
root, never over an existing entry). `file_write` takes a header `{root, path, offset?,
truncate?}` and up to 1 MiB of raw data; without `offset` it appends. A picked file has
no paths inside it.

The host refuses absolute paths, `..`, `.`, empty parts, backslashes, `:` and other
characters Windows reserves, reserved device names, and any symbolic link under the
root. Changes apply at once, even if the invocation later fails. One invocation may write
and read at most 512 MiB each.

```json
{"pick": {"kind": "save_file", "purpose": "Export the archive", "name": "archive.json", "action": "exported"}}
```

## Media worker (preview)

With `media_worker`, a plugin declares one action with `surface: "media"`. While the user
is in a call, the host keeps one instance of the plugin loaded on its own thread, so its
memory carries state from one call of `serein_invoke` to the next, and runs the action
once per batch. The instance may grow to 64 MiB of memory, and each batch gets fresh fuel
(50,000,000) and host-call budgets.

The action receives `media`: `seq`, `dropped` (batches the host discarded because the
plugin fell behind), `data_bytes` and up to 256 `items` in time order:

- `{"type": "event", "time_us", "event"}` with `event.kind` of `started` (`channel_id`,
  `guild_id`; first), `joined` and `left` (`user_id`), or `ended` (last). `time_us` counts
  from the worker's start.
- `{"type": "chunk", "stream", "codec", "time_us", "offset", "length"}` points into the
  batch's binary data (at most 4 MiB), copied with `serein.media_read`. Media taps fill
  these; until then batches carry only events.

The action may return only `captions` (with `call_captions`), shown above the call
controls. It may also call the clock, randomness, its files, its network origins and
`broadcast`; the database, journal and topic deliveries are not available, and it cannot
save `storage`. The host queues at most 32 batches or 16 MiB per worker, dropping the
oldest. An error ends the worker; a new one starts 10 seconds later in the same call. The
worker ends with the call, or when the plugin is disabled or the account changes.

```json
{"action": "media", "media": {"seq": 2, "items": [
  {"type": "event", "time_us": 1520000, "event": {"kind": "joined", "user_id": "1200000000000000000"}}]}}
```

## Media taps (preview)

With `media_packets` or `media_audio` (each requires `media_worker`), the media worker's
batches also carry the call's media. The voice transport copies each packet right after
DAVE decryption, on its network task, into one queue of at most 4,096 packets or 16 MiB;
the copy happens only while such a worker runs. A separate thread drains the queue every
20 ms and builds each worker's batches.

- Streams are `voice:<user>`, `camera:<user>`, `screen:<user>` and `screen_audio:<user>`.
- `media_packets` chunks are one Opus frame (`opus`) or one complete access unit (`h264`,
  `h265`, `av1`), with the sender's `rtp_timestamp`, audio `sequence` and video
  `keyframe`. Video is copied only once the client accepted the picture, so a stream
  starts on a keyframe.
- `media_audio` chunks (`pcm_s16le`) are one Opus frame decoded to 16 kHz mono
  little-endian samples, one decoder per stream, off the UI thread.
- `{"type": "gap", "stream", "time_us", "lost"}` reports packets the tap could not keep.
  A larger-than-4 MiB picture also becomes a gap.
- A `media` result with `media_packets` may return `keyframes`: up to 16 `camera:<user>`
  or `screen:<user>` streams. The host asks those senders through the same rate-limited
  path it uses to recover lost pictures.

Not included: the user's own microphone and camera, audio while the user is deafened
(it is not decrypted then), and video Discord does not send, such as streams the user is
not watching.

## Attachment archive (preview)

With `media_archive` (and `files`), a plugin saves original attachments, for example the
ones its message journal reports, before their signed links expire or the message is
deleted. `serein.attachment_fetch` takes `{"url", "size", "path"}` and returns at once;
the host downloads the file afterwards, one at a time across plugins, into `path` in the
plugin's private folder.

- **Links**: only signed original attachment links on `cdn.discordapp.com` (a
  `media.discordapp.net` link is rewritten to its original), with the same admission as
  the client's own downloads: HTTPS on port 443, no credentials or fragment, the
  `attachments/<channel>/[<message>/]<attachment>/<file>` path, and only the `ex`, `is`,
  `hm` and `backend` parameters. No redirects are followed and no credentials are sent.
- **Size**: `size` is the attachment's size, 1 byte to 100 MiB. The download must match it
  exactly; anything else fails. Each file must fit the plugin's data limit before it
  starts.
- **Writing**: the file is written as `<path>.part` and renamed once complete; an existing
  file is never replaced. `path` follows the private folder's path rules.
- **Queue**: at most 256 waiting fetches per plugin (1,024 in all) and one per path.
  `{"op": "fetches"}` through `serein.file_request` returns the last 256 outcomes as
  `{"path", "state": "queued" | "done" | "failed", "error"?}`. Disabling the plugin or
  logging out of its account drops its waiting fetches, stops the running one and forgets
  its outcomes.

Expired links are not refreshed; a fetch of one fails with "its link may have expired".

## Plugin data (preview)

Plugin data lives apart from packages in `extensions/plugin-data/<account>/<plugin id>/`
(`connection/` for connection plugins): `data.json` (`storage`), `data.sqlite`
(`database`), `files/` (`files`) and `settings.json` (the chosen data size and the
folders and files the user picked). The account's message
journal, approved connections (`subscriptions.json`) and message hooks (`hooks.json`)
sit beside them. Data from older versions inside package directories moves
there on first use.

- **Updating** a plugin keeps its data.
- **Disabling** a plugin that has data asks whether to keep or delete it.
- **Logging out** removes the account's plugin packages and grants but keeps their data.
- **Reinstalling** the same plugin ID in the same account picks up its kept data.
- Settings > Extensions lists **kept plugin data** of plugins that are not installed, with
  its size and a delete action; a plugin's details show its data size and, for `database`
  plugins, the database size limit.

Kept data is ordinary local data, not encrypted by Serein, and is never uploaded.

## API proxy plugin (preview)

The optional API Proxy plugin uses the native extension panel to select Direct,
Automatic (environment variables), or a custom HTTP/HTTPS proxy origin. An explicit Apply action changes
REST routing and saves its configuration with the separately granted `storage`
capability. No calls, voice sockets, Gateway WebSockets, CDN/media fetches or
external extension downloads are proxied by this capability.

This is a connection plugin, available in Settings > Extensions before login.
Consent applies across accounts on this device. Its package and bounded local
storage live in `extensions/proxy-plugins`, survive account switching/logout, and
are removed when explicitly disabled. Other plugins keep their account isolation.
A manifest requesting `api_proxy` must be a plugin with only `api_proxy` and
optional `storage`, and only panel/activation actions. It receives no account data,
tokens or requests and has no networking API.

Custom proxy origins must use HTTP or HTTPS, have a host, and be at most 2048 bytes.
Credentials, paths beyond `/`, queries and fragments are rejected. Serein can attach HTTP Basic proxy credentials from its host-managed masked form
and OS credential store. Credentials are never passed to Wasm or saved in plugin data.
SOCKS and PAC are unsupported. REST requests keep TLS certificate verification.
A failed configured proxy has no direct fallback. Invalid plugin configuration
blocks initial API routing; reload/handler errors retain the previous valid route
until the plugin is repaired or explicitly disabled.

Automatic reads `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY` and their lowercase
equivalents, respecting `NO_PROXY`. With none configured it connects directly.
OS/browser proxy settings and PAC discovery are unsupported.
