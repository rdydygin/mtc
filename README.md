<img src="assets/icon.svg" width="64" height="64" alt="">

# mtc

A fast, keyboard-first Mattermost desktop client built with [GPUI Kit](https://gpui-kit.com).

```sh
cargo run --release -p mtc
```

Sign in with your server address and a password, a personal access token, or your
company's single sign-on.

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

If the browser can't hand control back, copy the address of the page it ends on
(`…/login/desktop?client_token=…&server_token=…`) and paste it into mtc. On older
servers, copy the "click here" link on the final page (`mmauth://callback?…`) instead. Servers
without a license hide some providers from their public settings;
`MTC_SSO_PROVIDERS=gitlab,saml` shows them anyway.

## Using it

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
next to the standard ones.

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
- **Virtualized timelines.** Posts render through `MessageScroller`. `Timeline` emits
  splice events (`Spliced { old, new_count }`, `Updated(ix)`), so a new post or an edit
  measures one row, not the whole list.
- **Work happens once, at ingest.** Emoji shortcodes, autolinks, and the plain vs rich
  decision run when a post arrives, never during render. Custom emoji load in the
  background at sign-in; when they arrive, open timelines re-prepare only the posts that
  name one. Most chat messages are plain
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
mentions and DMs when the conversation isn't in view (the app must run as a `.app`
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

- The session token is stored in `~/Library/Application Support/mtc/state.json` with
  owner-only permissions. Moving it to the Keychain is a natural next step.
- Not implemented yet: message search, @mention autocomplete, multiple
  servers.

## Development

```sh
cargo test --workspace     # client parsing, message preparation, Timeline behavior
cargo clippy --workspace --all-targets
# Live SSO exchange against a server, after running the browser leg yourself:
MTC_SSO_SERVER=… MTC_SSO_CALLBACK='…/login/desktop?…' cargo test -p mattermost -- --ignored
```

To run a throwaway server locally:
`docker run --platform linux/amd64 -d --name mtc-mattermost -p 8065:8065 mattermost/mattermost-preview`.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
