# Disc CLI

Native Rust CLI for **Disc** – discover signals and consume live data streams.

---

## Quick start (30 seconds)

```bash
brew install disctechnologies/tap/disc

# approve this machine in your browser
disc auth login

# stream a signal
disc signals passive subscribe <passive-signal-id> --format ndjson
```

---

## What it does

- 🔍 Discover passive and active signals
- 📡 Subscribe to live signal streams (WebSocket)
- 🔐 Authenticate in the browser with a subject-bound API key
- ⚡ Stream data to stdout (pipe-friendly)

Backed by:
- HTTP: `api.disc.tech`
- WS: `signals.disc.tech` (MessagePack protocol)

---

## Installation

### Homebrew (recommended)

```bash
brew tap disctech/tap
brew install disc
```

Verify:

```bash
disc --version
```

---

## Authentication

Sign in and approve the requested Disc subject in your browser:

```bash
disc auth login
```

For a remote or headless terminal, open the printed URL on another device:

```bash
disc auth login --no-browser
```

Each browser login creates or replaces a local subject profile. List and switch profiles with:

```bash
disc auth list
disc auth use <profile>
```

Clear the active profile, or every profile:

```bash
disc auth clear
disc auth clear --all
```

Manual API-key setup remains available for automation and recovery:

```bash
disc auth api-key set
```

Or pass per command:

```bash
DISC_API_KEY=... disc auth whoami
```

Check current auth:

```bash
disc auth whoami
```

---

## Discover signals

### Passive signals

```bash
disc signals passive list
disc signals passive get <passive-signal-id>
```

### Active signals

```bash
disc signals active list --for-passive <passive-signal-id>
disc signals active get <active-signal-id>
```

---

## Stream live data

### Subscribe (machine-friendly)

Streams events to stdout (best for piping):

```bash
disc signals passive subscribe <passive-signal-id> --format ndjson
```

Pipe to another process:

```bash
disc signals passive subscribe <passive-signal-id> --format ndjson | jq
```

Write to file:

```bash
disc signals passive subscribe <passive-signal-id> \
  --format ndjson \
  --destination ./output.ndjson
```

With backfill:

```bash
disc signals passive subscribe <passive-signal-id> \
  --backfill \
  --backfill-count 5 \
  --format ndjson
```

---

### Tail (human-friendly)

Pretty console output:

```bash
disc signals active tail <active-signal-id> --format pretty
```

---

### Interactive mode

```bash
disc signals subscribe
```

- toggle passive signals
- explore active signals
- manage live subscriptions
- stream to file

---

## Runtime options

### Streaming

- `--window-semantics elapsed|ordinal`
- `--backfill`
- `--backfill-count <n>`
- `--backfill-from <epoch-ms>`
- `--backfill-to <epoch-ms>`
- `--include-status`
- `--once`
- `--timeout <duration>`
- `--no-reconnect`

### Output formats

- `pretty`
- `json`
- `ndjson` (recommended for pipelines)

### Output filters

- `data`
- `status`
- `events`
- `all`

---

## Configuration

Stored in platform-standard locations:

- macOS: `~/Library/Application Support/disc/`
- Linux: `${XDG_CONFIG_HOME:-~/.config}/disc/`
- Windows: `%APPDATA%/disc/`

Files:

- `config.json`
- `auth.json`

`auth.json` stores subject profiles and their API keys with owner-only permissions. API keys are never committed to the
repo.

---

## Development

Build locally:

```bash
cargo build
```

Run:

```bash
cargo run --bin disc -- --help
```

Run the behavioural test suite:

```bash
make test
```

The coverage gate requires at least 93% line, region, and function coverage:

```bash
make test-cov
```

### Local wrapper

```bash
./disc.sh auth whoami
```

Defaults:

- HTTP: `http://localhost:3001`
- WS: `ws://localhost:8097`
- Client ID: `disc-cli-local`

Env precedence:

1. `DISC_LOCAL_API_KEY`
2. `DISC_API_KEY`
3. stored CLI auth

---

## Release & distribution (maintainers)

`disc-cli` is distributed as **prebuilt binaries** via GitHub Releases and installed via Homebrew.

Create a release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

Artifacts:

- `disc-<target>.tar.gz`
- `SHA256SUMS.txt`
- `disc.rb` (Homebrew formula)

---

## Design principles

- 🧩 Native Rust binary (no runtime dependencies)
- 🔌 Unix-first (stdout streaming, pipe-friendly)
- ⚡ Low-latency real-time consumption
- 🧱 Stable CLI interface over evolving backend

---

## License

See `LICENSE`.
