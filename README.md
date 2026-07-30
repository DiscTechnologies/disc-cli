# Disc CLI

Native Rust CLI for **Disc** – discover signals and consume live data streams.

---

## Quick start (30 seconds)

```bash
brew install disctechnologies/tap/disc

# sign in through your browser
disc login

# stream a signal
disc signals passive subscribe <passive-signal-id> --format ndjson
```

---

## What it does

- 🔍 Discover passive and active signals
- 📡 Subscribe to live signal streams (WebSocket)
- 🔐 Authenticate with OAuth Authorization Code + PKCE or the Device Authorization Grant
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

Sign in through Keycloak with Authorization Code + PKCE. The CLI opens an ephemeral
`127.0.0.1` callback and asks you to choose an eligible Disc product subject:

```bash
disc login
```

For a remote or headless terminal, use the standard OAuth device flow:

```bash
disc login --device
```

With one eligible subject, Disc selects it automatically. With multiple eligible subjects,
the CLI asks which one to use. Use `--no-browser` to print the PKCE authorization URL without
launching a browser, or `--subject <id-or-key>` for deterministic non-interactive selection.

Production API, WebSocket, SSO issuer, and OAuth client values are built in. Environment variables
and command-line options remain available as explicit overrides for local development,
self-hosting, automation, and debugging. The longer `disc auth login` spelling remains supported
for compatibility.

Each login creates or replaces one local subject profile. List and switch profiles with:

```bash
disc auth list
disc auth use <profile>
```

Revoke the active OAuth session remotely and remove its local credentials:

```bash
disc auth logout
disc auth logout --all
```

`disc auth clear` is intentionally limited to manual API-key profiles; OAuth profiles must use
logout so local deletion cannot silently skip server-side revocation.

Manual API-key setup remains available as an explicit automation compatibility path:

```bash
disc auth api-key set
```

Or pass per command:

```bash
DISC_API_KEY=... disc auth whoami
```

Check current auth (`status` is an alias):

```bash
disc auth whoami
disc auth status
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

`auth.json` is atomically written with owner-only permissions. OAuth profiles store issuer, public client, user, and
subject metadata plus an opaque credential-store account reference. Rotating refresh tokens are stored only in the
operating-system credential store (macOS Keychain, Windows Credential Manager, or the platform Linux secret service).
There is no plaintext fallback. Manual API-key profiles remain in `auth.json` for backwards-compatible automation.

OAuth access tokens are short lived and refreshed under a cross-process lock. HTTP calls send the access token and a
fresh subject-context token. WebSocket connections mint a 30-second single-use ticket so OAuth bearer material is never
placed in a WebSocket subprotocol.

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

Production validation also requires:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --release --locked
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
Release CI builds native executables for Apple Silicon and Intel macOS, x86-64 Linux, and
x86-64 Windows. Every archive is included in the published GitHub release and covered
by `SHA256SUMS.txt`. GitHub Actions also publishes signed SLSA build-provenance
attestations for every archive. Homebrew consumes only the macOS and Linux archives.

Create a release:

```bash
git tag v0.1.0
git push origin v0.1.0
```

Artifacts:

- `disc-<target>.tar.gz`
- `SHA256SUMS.txt`
- `disc.rb` (Homebrew formula)

The Windows archive contains `disc.exe`; Unix archives contain `disc`.

Verify a downloaded archive's checksum and signed provenance:

```bash
shasum -a 256 --check --ignore-missing SHA256SUMS.txt
gh attestation verify disc-<target>.tar.gz --repo DiscTechnologies/disc-cli
```

---

## Design principles

- 🧩 Native Rust binary (no runtime dependencies)
- 🔌 Unix-first (stdout streaming, pipe-friendly)
- ⚡ Low-latency real-time consumption
- 🧱 Stable CLI interface over evolving backend

---

## License

See `LICENSE`.
