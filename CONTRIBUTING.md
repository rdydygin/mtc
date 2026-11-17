# Contributing

Work on a short-lived feature branch, open a pull request into `main`, and merge after
all required CI checks pass. Merging does not publish a release.

## Checks

```sh
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
```

CI (`.github/workflows/ci.yml`) runs on PRs and pushes to `main`, and can be run
manually. It checks rustfmt on Ubuntu, clippy and tests on macOS, and builds, clippy
and tests on Linux and Windows, all on stable Rust with `--locked` and warnings denied.
GPUI tests use the in-process test platform, so no display server or GPU is needed.

On Linux and Windows, `MTC_ALLOW_MULTIPLE=1 cargo run -p mtc` bypasses the
single-instance profile lock. Use it only with throwaway profile data.

## A local server

```sh
docker run --platform linux/amd64 -d --name mtc-mattermost -p 8065:8065 mattermost/mattermost-preview
```

## Live server tests

REST/WebSocket contracts and GPUI session flows can run against a disposable
Mattermost server:

```sh
script/live-test   # Docker + curl; starts or reuses mtc-live-mattermost
# Or point at an already running disposable server:
MTC_LIVE_SERVER=http://localhost:8065 cargo test --locked -p mattermost --test live -- --ignored --test-threads=1
MTC_LIVE_SERVER=http://localhost:8065 cargo test --locked -p session --test live -- --ignored --test-threads=1
# Delete all local test data:
docker rm -f mtc-live-mattermost
```

- The script and the informational **Live Mattermost contract tests** CI job pin
  `mattermost/mattermost-preview:10.11.26`.
- The tests create the bootstrap admin `mtc_live_admin` / `Live-test-Password9!`, then
  uniquely named users, a team and channels on every run. Data is kept for inspection.
- **They change server configuration** (open signup, no email verification, collapsed
  threads always on, commands, custom emoji and custom statuses). Point them only at
  disposable instances.
- Without `MTC_LIVE_SERVER`, they skip with a message even under `--ignored`.
- A recording proxy captures each request, response and WebSocket event, printed on
  failure (or always with `--nocapture`). Credentials are omitted.

A live SSO exchange, after running the browser leg yourself:

```sh
MTC_SSO_SERVER=… MTC_SSO_CALLBACK='…/login/desktop?…' cargo test -p mattermost -- --ignored
```

## Slow networks

`script/slow-network` sits between mtc and a server and adds latency and a bandwidth
cap to everything, WebSocket included. Sign in to `http://localhost:8066`:

```sh
script/slow-network --upstream localhost:8065 --profile slow-3g --log   # 400 ms, 400 kbit/s
script/slow-network --upstream localhost:8065 --rtt 800 --kbps 120      # your own numbers
```

`--log` prints each request, showing the order and overlap of startup requests. The
automated tests use `mattermost::testing::SlowServer` (feature `test-support`) the same
way.

Measured against a real server with an 80-channel team, from the first request until
the last conversation's messages are on screen:

| Link | Before | After |
| --- | --- | --- |
| Slow 3G: 400 ms, 400 kbit/s | 3.8 s | 0.5 s |
| EDGE: 800 ms, 120 kbit/s | 11.5 s | 1.0 s |

## Releasing

Push a `vMAJOR.MINOR.PATCH` tag:

```sh
git tag v0.3.0 && git push origin v0.3.0
```

Or use **Actions → Release → Run workflow**. A branch run takes an optional `version`;
when empty it uses the next patch after the newest tag, or the `Cargo.toml` workspace
version if higher. Bump that version to start a new minor or major series. Select
`dry_run` (or `gh workflow run release.yml --ref main -F dry_run=true`) to build and
verify without publishing anything.

The Release workflow:

1. reruns full CI on the exact commit,
2. stamps the version into the checkout (nothing is committed) and builds a universal
   `mtc.app` with `script/bundle-macos --universal`,
3. packages a zip, its SHA-256 and `latest.json`, and verifies them in a read-only job,
4. creates a draft release, uploads the assets, checks the tag still points at the
   built commit, and publishes.

Existing releases, including drafts, are refused; if publishing fails, inspect and
delete the draft before retrying.

### The update feed

Installed copies read
`https://github.com/rdydygin/mtc/releases/latest/download/latest.json`, which GitHub
redirects to the newest published release, so publishing is all it takes to ship an
update. The feed, downloads and every redirect must use HTTPS.

To try an update end to end, bump the version, bundle it, serve a `latest.json` that
points at its zip, and start an older **debug** bundle (`script/bundle-macos --debug`)
with the override:

```sh
MTC_UPDATE_FEED=http://127.0.0.1:8000/latest.json target/debug/bundle/mtc.app/Contents/MacOS/mtc
```

Only debug builds honor `MTC_UPDATE_FEED`, and only they may use plain HTTP, to
loopback addresses. Release builds always use the official feed.
