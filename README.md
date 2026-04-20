# disc-cli

Native Rust CLI for Disc signal discovery and live signal consumption.

The current v1 scope is aligned with the live Disc signal stack:

- HTTP discovery via `api.disc.tech`
- WebSocket subscriptions via `signals.disc.tech`
- API-key authentication via `X-Disc-Api-Key`
- MessagePack websocket protocol compatible with the current Disc signals services

## Build

```bash
cargo build
```

Run directly during development:

```bash
cargo run --bin disc -- --help
```

Use the thin local-stack wrapper:

```bash
./disc.sh auth whoami
```

The wrapper pins local defaults:

- HTTP: `http://localhost:3001`
- WS: `ws://localhost:8097`
- client id: `disc-cli-local`

If `DISC_LOCAL_API_KEY` is set, the wrapper injects it automatically. If it is not set, it falls back to `DISC_API_KEY`, and otherwise uses whatever auth state the CLI resolves on its own.

## Auth

Store an API key locally:

```bash
cargo run --bin disc -- auth api-key set
```

Or pass it per-command:

```bash
DISC_API_KEY=... cargo run --bin disc -- auth whoami
```

Validate the configured credential:

```bash
cargo run --bin disc -- auth whoami
```

## Signal discovery

List passive signals:

```bash
cargo run --bin disc -- signals passive list
```

Get one passive signal:

```bash
cargo run --bin disc -- signals passive get <passive-signal-id>
```

List active signals for a passive signal:

```bash
cargo run --bin disc -- signals active list --for-passive <passive-signal-id>
```

Get one active signal:

```bash
cargo run --bin disc -- signals active get <active-signal-id>
```

## Live streaming

`subscribe` is the machine-oriented path:

- silently maintains the websocket subscription
- forwards matched events to a destination
- defaults to `ndjson`

`tail` is the human-oriented path:

- pretty-prints live events to the console
- includes subscription lifecycle output by default

Subscribe to one passive signal and forward to stdout:

```bash
cargo run --bin disc -- signals passive subscribe <passive-signal-id>
```

Subscribe with backfill and append to a file:

```bash
cargo run --bin disc -- signals passive subscribe <passive-signal-id> \
  --backfill \
  --backfill-count 5 \
  --include-status \
  --format ndjson \
  --destination ./passive-signal.ndjson
```

Tail one active signal in the console:

```bash
cargo run --bin disc -- signals active tail <active-signal-id> \
  --window-semantics ordinal \
  --format pretty
```

Interactive subscription manager:

```bash
cargo run --bin disc -- signals subscribe
```

That opens a persistent prompt where you can:

- toggle passive signal subscriptions
- pick a passive signal and expand its active signals
- toggle active signal subscriptions
- keep subscriptions running and write them to the chosen destination file

## Runtime options

Supported stream options:

- `--window-semantics elapsed|ordinal`
- `--backfill`
- `--backfill-count <n>`
- `--backfill-from <epoch-ms>`
- `--backfill-to <epoch-ms>`
- `--include-status`
- `--once`
- `--timeout <duration>`
- `--no-reconnect`

Supported output modes:

- `pretty`
- `json`
- `ndjson`

Supported output filters:

- `data`
- `status`
- `events`
- `all`

## Local config

The CLI stores config/auth in platform-standard config directories:

- macOS: `~/Library/Application Support/disc/`
- Linux: `${XDG_CONFIG_HOME:-~/.config}/disc/`
- Windows: `%APPDATA%/disc/`

Tracked files are:

- `config.json`
- `auth.json`

The API key is never written into repository files.

## Homebrew release flow

`disc-cli` is packaged for Homebrew as prebuilt release archives, not source builds on the user machine.

Release archives contain only:

- `disc`
- `README.md`
- `LICENSE`

Release automation lives in `.github/workflows/release.yml` and currently targets:

- `aarch64-apple-darwin` on `macos-14`
- `x86_64-apple-darwin` on `macos-13`
- `x86_64-unknown-linux-gnu` on `ubuntu-24.04`

Create a release tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The workflow will publish:

- `disc-aarch64-apple-darwin.tar.gz`
- `disc-x86_64-apple-darwin.tar.gz`
- `disc-x86_64-unknown-linux-gnu.tar.gz`
- `SHA256SUMS.txt`
- `disc.rb`

Local packaging smoke test:

```bash
cargo build --locked --release --bin disc
./scripts/package-release.sh \
  --binary ./target/release/disc \
  --target "$(rustc -vV | sed -n 's/^host: //p')" \
  --output-dir ./dist
```

Local formula rendering smoke test:

```bash
./scripts/package-release.sh \
  --binary ./target/release/disc \
  --target aarch64-apple-darwin \
  --output-dir ./dist
./scripts/package-release.sh \
  --binary ./target/release/disc \
  --target x86_64-apple-darwin \
  --output-dir ./dist
./scripts/package-release.sh \
  --binary ./target/release/disc \
  --target x86_64-unknown-linux-gnu \
  --output-dir ./dist
shasum -a 256 ./dist/disc-*.tar.gz > ./dist/SHA256SUMS.txt
python3 ./scripts/render_homebrew_formula.py \
  --version 0.1.0 \
  --release-base-url https://github.com/disctech/disc-cli/releases/download/v0.1.0 \
  --checksums ./dist/SHA256SUMS.txt \
  --output ./dist/disc.rb
```

Publish to the tap after the GitHub release completes:

```bash
brew tap disctech/tap
brew install disctech/tap/disc
disc --version
```
