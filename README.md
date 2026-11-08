<img src="assets/icon.svg" width="64" height="64" alt="">

# mtc

A fast, keyboard-first Mattermost desktop client built with [GPUI Kit](https://gpui-kit.com).

Download `mtc.app` for macOS from [Releases](https://github.com/rdydygin/mtc/releases), or build it:

```sh
cargo run --release -p mtc
```

Sign in with your server address and a password, a personal access token, or your
company's single sign-on.

### Updates

mtc keeps itself up to date. A few seconds after launch, and every six hours while it
runs, it looks for a new release. On macOS it downloads the new `mtc.app` in the
background, checks it against the release's SHA-256 checksum, its bundle identifier,
version, and signature, and puts it in place of the running copy. The new version
starts the next time you open mtc, or right away with **Restart to update** in the
title bar.

To install itself, mtc must run from a folder it can write to, such as `/Applications`
or `~/Applications`. A copy opened straight from Downloads runs from a read-only
location until it is moved; that copy, builds you made yourself, and Linux and Windows
show **Update available** instead, linking to the release.

**Settings** (⌘,) shows the version you're running under **Updates**, with a **Check for
updates** button that reports the result in place: up to date, a new version, or why the
check failed. **Check for Updates** in the mtc menu (or ⌘K) checks too. Turn off **Update
automatically** in Settings, or uncheck it in the mtc menu, to only check when you ask.

### Single sign-on (SAML, OpenID, Microsoft, Google, GitLab)

Type your server address and mtc shows a "Continue with …" button for each SSO method
the server offers. Sign-in happens in your default browser; Mattermost then hands a
one-time code back to mtc through a `mattermost-dev://` link, and mtc trades it for a
session. The official Mattermost app keeps `mattermost://`, so the two don't collide.

Servers older than 9.1 don't have that hand-back, so mtc uses the mobile apps' flow
instead: the browser returns an `mmauth://` link that carries the session itself. mtc
picks the flow from the server's version and only accepts the link while it is waiting
for a sign-in.

macOS only delivers these links to an installed app bundle, so for SSO run mtc as an app:

```sh
script/bundle-macos --install   # builds mtc.app into ~/Applications and registers it
open ~/Applications/mtc.app
```

On Linux and Windows mtc registers the links itself each time it starts, pointing them
at the executable you ran. On Linux that's a hidden `mtc-sso-callback.desktop` entry in
`~/.local/share/applications`, set as the handler with `xdg-mime`. On Windows it's a
per-user key under `HKEY_CURRENT_USER\Software\Classes`. When the browser opens a link,
the new mtc process passes it to the window that's already running and exits.

If the browser can't hand control back, copy the address of the page it ends on
(`…/login/desktop?client_token=…&server_token=…`) and paste it into mtc. On older
servers, copy the "click here" link on the final page (`mmauth://callback?…`) instead. Servers
without a license hide some providers from their public settings;
`MTC_SSO_PROVIDERS=gitlab,saml` shows them anyway.

## Using it

On Linux and Windows, use Ctrl where the table says ⌘, Alt for ⌥, and Shift for ⇧.

| Shortcut | Action |
| --- | --- |
| ⌘K / ⌘T | Jump to a channel, person, or app command |
| ⌥↑ / ⌥↓ | Previous / next channel |
| ⌥⇧↑ / ⌥⇧↓ | Previous / next unread channel |
| Enter / ⇧Enter | Send / new line |
| ↑ in an empty field | Edit your last message |
| Tab | Complete a slash command |
| Esc | Close the thread, or mark the channel read |
| ⌘L | Focus the message field |
| ⌘W | Close the conversation (keeps its draft) |
| ⌘= / ⌘- / ⌘0 | Zoom in / out / actual size |
| ⌘/ | All shortcuts |
| ⌘, | Settings: appearance, text size, updates, version |

Attach files with the paperclip, by dropping them on a conversation, or by pasting an
image; they upload right away and send with the message. Click an attachment to download
it to Downloads and open it.

Right-click a message to reply, react, copy, edit, or delete it. Click a message to open
its replies in a thread panel beside the channel (collapsed reply threads); the reply
count sits at the end of the message's reactions line. Click someone's name or avatar to
see their profile and start a direct message.

Star a conversation from the star in its header, by right-clicking it in the sidebar, or
with "Add to favorites" in ⌘K. Favorites sit in their own section at the top of the
sidebar and sync with your other Mattermost apps.

The server's custom emoji show up inline in messages, in reactions, and in custom statuses,
next to the standard ones. The emoji on screen are looked up by name as they appear, so
they show even on servers with thousands of custom emoji, or where the full list can't
be read.

## Architecture

```text
crates/
├── mattermost/   REST + WebSocket client. No UI. Executor-agnostic.
├── session/      The signed-in state as GPUI entities. No rendering.
│   ├── Session     teams, channels, read state, event routing, sending
│   ├── Directory   users and presence, batched lookups
│   └── Timeline    ordered posts of one channel or thread
└── app/          The window: sign-in, sidebar, timelines, composer, ⌘K
```

Dependencies point downward. The app depends on `gpui-kit` alone for UI.

### Performance

- **No I/O on the UI thread.** The client runs requests, JSON decoding, and the
  WebSocket on a private tokio runtime. Its futures can be awaited from GPUI's executor.
  Saved state is read once and written by a background thread, so neither rendering
  nor switching channels waits on the disk.
- **Startup in one round trip.** A restored session opens the window from the profile
  saved last time and sends every startup request at once: the remembered team's
  channels, the last conversation's posts, favorites, emoji, and a check of the token.
  (They used to wait on one another: profile, then teams, then channels, then posts.)
  Profiles and presence for a page of authors arrive together, too.
- **Compressed responses.** Requests accept gzip, Brotli, zstd, and deflate. Mattermost
  compresses its JSON about fifteenfold (a team's 80-channel list: 70 KB → 4.6 KB).
- **Built for slow links.** Timeouts measure silence, not size, so a large download
  on a slow connection finishes while a dead connection fails within 30 seconds.
  Startup loads that fail for reasons that may pass (a timeout, a 5xx) are retried
  with backoff, and again once the socket reconnects.
- **Skeletons while loading.** The window, sidebar, and conversation show their shape
  while their content is on the way, with the same lanes as the real rows, so
  nothing moves when it arrives.
- **Virtualized timelines.** Posts render through `MessageScroller`. `Timeline` emits
  splice events (`Spliced { old, new_count }`, `Updated(ix)`), so a new post or an edit
  measures one row, not the whole list.
- **Work happens once, at ingest.** Emoji shortcodes, autolinks, and the plain vs rich
  decision run when a post arrives, never during render. Custom emoji are looked
  up by name as posts, reactions, and statuses name them, batched like users, while the
  full list loads in the background; when they arrive, open timelines re-prepare only
  the posts that name one. Most chat messages are plain
  text and skip Markdown entirely. Rich posts keep their parsed state per post, so
  scrolling back never re-parses.
- **Instant switching.** The 8 most recent channel views stay alive with their scroll
  position, parsed text, and draft. Timelines are cached a little longer (24).
- **Batched lookups.** Author lookups requested within one frame become a single
  `users/ids` call.
- **Avatars use GPUI's image cache.** An authenticated HTTP client (`app/src/http.rs`)
  attaches the token only for the signed-in server. Decoding happens off the UI thread.
- **Resilient real-time.** The socket detects silent connections, reconnects with
  backoff, and catches up live timelines with `posts?since=`.

### Effortless UX

Posts send optimistically, with Retry or Discard if a send fails. Drafts are kept per
conversation, and a "New messages" divider marks where you left off. You also get
typing indicators, presence dots, and mention badges. Native notifications cover
mentions and DMs when the conversation isn't in view (on macOS the app must run as a `.app`
bundle for these). The app restores your session, team, last channel, zoom, appearance,
and sidebar width on launch, and follows the system light/dark setting.

### Extensibility

`app/src/extensions.rs` defines the `Extension` trait. An extension can:

- add slash commands (`SlashCommand::new(..).on_run(..)`, returning `Send`, `Handled`,
  or `Pass` to the server),
- render particular posts its own way (`render_post_body`),
- observe session events, including server events this client doesn't model
  (`SessionEvent::Raw`).

The built-in behavior (`/shrug`, `/me`, `/theme`, `/read`, system-message rendering,
notifications) is itself registered as the `Core` extension.

## Notes

- The session token is stored in `state.json` in the platform config directory
  (`~/Library/Application Support/mtc` on macOS, `~/.config/mtc` on Linux,
  `%APPDATA%\mtc` on Windows). On macOS and Linux the file is owner-only; on Windows
  it relies on your profile folder's permissions. Moving it to the system keychain is a
  natural next step.
- Not implemented yet: message search, @mention autocomplete, multiple
  servers.

## Development

```sh
cargo test --workspace     # client parsing, message preparation, Timeline behavior
cargo clippy --workspace --all-targets
# Live SSO exchange against a server, after running the browser leg yourself:
MTC_SSO_SERVER=… MTC_SSO_CALLBACK='…/login/desktop?…' cargo test -p mattermost -- --ignored
```

CI (`.github/workflows/ci.yml`) runs rustfmt, clippy with warnings denied, and the tests on
every pull request and push to `main`. To publish a release, bump `version` in `Cargo.toml`,
then either run the Release workflow from the Actions tab (it tags the branch as
`v<version>`) or push a matching tag (`git tag v0.1.0 && git push origin v0.1.0`):
`.github/workflows/release.yml` builds a universal (Apple Silicon and Intel) `mtc.app` with
`script/bundle-macos --universal` and attaches it to a GitHub release.

The release also carries `latest.json`, the update feed: the version, the release page,
and each download's URL and SHA-256. Installed copies read it from
`https://github.com/rdydygin/mtc/releases/latest/download/latest.json`, which GitHub
redirects to the newest release that isn't a draft or pre-release, so publishing a
release is all it takes to ship an update. Release downloads, unlike the GitHub API,
aren't rate-limited per address. To try an update end to end, bump the version, bundle
it, serve a `latest.json` that points at its zip, and start an older build with
`MTC_UPDATE_FEED=http://…/latest.json` (debug builds only check on their own when it is
set).

To run a throwaway server locally:
`docker run --platform linux/amd64 -d --name mtc-mattermost -p 8065:8065 mattermost/mattermost-preview`.

### Testing on a slow network

`script/slow-network` sits between mtc and a server and adds latency and a bandwidth
cap to everything that passes, WebSocket included. Sign in to `http://localhost:8066`
instead of the server's address:

```sh
script/slow-network --upstream localhost:8065 --profile slow-3g --log   # 400 ms, 400 kbit/s
script/slow-network --upstream localhost:8065 --rtt 800 --kbps 120      # your own numbers
```

`--log` prints each request as it passes, so the order and overlap of startup requests
is visible. The automated tests do the same in-process: `mattermost::testing::SlowServer`
(feature `test-support`) is a stand-in server with latency and bandwidth, used to check
that startup takes one round trip, that slow downloads finish and stalled ones fail,
and that startup survives failed requests.

Measured against a real server with an 80-channel team, from the first request until
the last conversation's messages are on screen:

| Link | Before | After |
| --- | --- | --- |
| Slow 3G: 400 ms, 400 kbit/s | 3.8 s | 0.5 s |
| EDGE: 800 ms, 120 kbit/s | 11.5 s | 1.0 s |

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
