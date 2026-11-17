<img src="assets/icon.svg" width="64" height="64" alt="">

# mtc

A fast, keyboard-first Mattermost desktop client built with [GPUI Kit](https://gpui-kit.com).

## Install

Download `mtc-vX.Y.Z-macos-universal.zip` from [Releases](https://github.com/rdydygin/mtc/releases).
It runs on Apple Silicon and Intel Macs (macOS 12 or later).

The app is ad-hoc signed, not notarized. After unzipping, move `mtc.app` to
Applications, then either right-click → **Open** the first time or run:

```sh
xattr -dr com.apple.quarantine /Applications/mtc.app
```

On Linux and Windows, build from source:

```sh
cargo run --release -p mtc
```

Sign in with your server address and a password, a personal access token, or single
sign-on (SAML, OpenID, Microsoft, Google, GitLab).

## Features

- **Fast on any link.** One-round-trip startup, compressed responses, virtualized
  timelines and sidebar, and no I/O on the UI thread. On slow 3G, the last conversation
  is on screen in 0.5 s (3.8 s before).
- **Keyboard everywhere.** ⌘K finds channels, people and commands; every pane is
  reachable and navigable by keyboard.
- **Composer.** Optimistic sends with Retry/Discard, drafts kept per conversation across
  restarts, file attachments (paperclip, drop or paste), and completion for `@people`,
  `~channels`, `:emoji:` and `/commands` (including server commands).
- **Threads.** Collapsed reply threads open beside the channel; the **Threads** view
  lists the threads you follow with unread and mention counts.
- **Search** the current conversation (⌘F) or the whole team (⌘⇧F) with Mattermost's
  search syntax.
- **Saved and pinned messages**, reactions with a searchable emoji picker, custom emoji,
  favorites, muting, mark as unread, group messages, and channel browsing.
- **Integrations.** Bot and webhook attachments render with fields, pictures and
  Markdown. Interactive buttons and menus open in the browser.
- **In-app links.** Mattermost permalinks and channel links on your server open in mtc.
- **Notifications** for mentions and DMs, honoring channel preferences, muting and
  Do Not Disturb.
- **Automatic updates** on macOS (see below).

Not implemented yet: multiple servers, interactive integration actions, and
"all messages" notifications.

## Keyboard shortcuts

On Linux and Windows, use Ctrl for ⌘, Alt for ⌥, and Shift for ⇧. ⌘/ lists them all.

| Shortcut | Action |
| --- | --- |
| ⌘K / ⌘T | Jump to a channel, person, or command |
| ⌘F | Search the conversation |
| ⌘⇧F | Search all channels in the team |
| ⌘⇧T | Threads you follow |
| ⌘⇧S | Saved messages |
| ⌘⇧P | Pinned messages in the channel |
| ⌥↑ / ⌥↓ | Previous / next channel |
| ⌥⇧↑ / ⌥⇧↓ | Previous / next unread channel |
| Enter / ⇧Enter | Send (or accept a suggestion) / new line |
| ↑ in an empty field | Edit your last message |
| Tab / Enter in suggestions | Complete the selected suggestion |
| Esc | Dismiss suggestions, cancel an edit, close the panel, or mark the channel read |
| ⌘L | Focus the message field |
| ⌘W | Close the conversation (keeps its draft) |
| ⌘= / ⌘- / ⌘0 | Zoom in / out / actual size |
| ⌘, | Settings |

Right-click a message to reply, react, save, pin, mark it unread, copy its text or
link, edit, or delete it. Right-click a conversation in the sidebar to favorite, mute,
or leave it.

## Updates

A few seconds after launch, and every six hours while running, mtc checks for a new
release. On macOS it downloads it in the background, verifies its SHA-256 checksum,
bundle identifier, version and signature, and swaps it in. The new version starts the
next time you open mtc, or right away with **Restart to update** in the title bar.

mtc can only update itself when it runs from a writable folder such as `/Applications`
or `~/Applications`. Copies run from Downloads, your own builds, and Linux and Windows
show **Update available** with a link to the release instead.

**Settings → Updates** shows your version and has **Check for updates**. Turn off
**Update automatically** there (or in the mtc menu) to check only when you ask.

## Single sign-on

Enter your server address and mtc shows a **Continue with …** button for each SSO
method the server offers. Sign-in happens in your browser, which hands control back to
mtc through a `mattermost-dev://` link (the official app keeps `mattermost://`).

- **macOS** only delivers these links to an installed app, so use the release build or
  `script/bundle-macos --install`.
- **Linux and Windows** register the link handler each time mtc starts.
- **Servers older than 9.1** use the mobile apps' `mmauth://` flow. Because those links
  aren't tied to your sign-in attempt, mtc shows the account and server and asks you to
  confirm before continuing.
- **If the browser can't hand back control**, copy the address of the page it ends on
  (`…/login/desktop?…`, or the `mmauth://callback?…` link on older servers) and paste
  it into mtc.
- Unlicensed servers may hide some providers; `MTC_SSO_PROVIDERS=gitlab,saml` shows
  them anyway.

## Data and privacy

- On macOS the session token lives in the login keychain (item `mtc`). Since releases
  are ad-hoc signed, macOS may ask after an update whether mtc may use it; choose
  **Always Allow**. On Linux and Windows the token is stored in `state.json`.
- Preferences, drafts and the rest of the session are in `state.json` in
  `~/Library/Application Support/mtc` (macOS), `~/.config/mtc` (Linux), or
  `%APPDATA%\mtc` (Windows), readable only by you on macOS and Linux.
- Drafts belong to the signed-in account and are discarded on sign-out. Attachments and
  failed sends are not kept across restarts.
- The session token is sent only to your signed-in server, never to external image
  hosts. Server SVG images render in a separate helper process.
- Downloaded programs, scripts, and unknown file types are revealed in the file manager
  rather than opened.

## Architecture

```text
crates/
├── mattermost/   REST + WebSocket client. No UI. Executor-agnostic.
├── session/      The signed-in state as GPUI entities. No rendering.
│   ├── Session          teams, channels, read state, event routing, sending
│   ├── Directory        users and presence, batched lookups
│   ├── Timeline         ordered posts of one channel or thread
│   └── FollowedThreads  the Threads pane's lists and unread totals
└── app/          The window: sign-in, sidebar, timelines, composer, ⌘K
```

Dependencies point downward; the app depends on `gpui-kit` alone for UI.
`app/src/extensions.rs` defines an `Extension` trait for slash commands, custom post
rendering, and raw server events. Built-ins such as `/shrug`, `/me`, `/theme` and
`/read` are registered as the `Core` extension.

## Development

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for live-server tests, slow-network testing,
testing updates, and the release process.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
