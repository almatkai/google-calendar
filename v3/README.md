# Google Calendar native process

Five standalone commands: create-event, create-quick-event, search-contacts,
list-events and list-calendars. This v3 crate leaves the original TypeScript
extension and its 23 tools intact. Schema 3 / JSONL protocol 1; immutable SDK git
revision is pinned in Cargo.toml and Cargo.lock. No app, Tauri, Bun or Node runtime.

Set TEZBAR_GOOGLE_OAUTH_CLIENT_ID to your own Google Desktop OAuth application's
client ID before launching the host process. This public identifier is not an API
key; no Raycast client, machine credentials, client secret or existing Raycast
tokens are used. Connect opens the consent URL through host.shell.open; loopback
127.0.0.1 uses an ephemeral port, state and S256 PKCE with a 180s callback deadline.
Commands allow 240s for authorization plus bounded HTTP calls. Enable Calendar and
People APIs and appropriate consent for the scopes in src/domain.rs.

Tokens live only in SDK dataDir/google-oauth/storage.json (atomic replacement,
corruption rejected, Unix directory 0700/file 0600). The host may explicitly grant
migrationPaths.nativeGoogleOAuth as the exact legacy native-google-oauth/tokens.json
file. It imports only access_token, refresh_token, expires_at, client_id, once.
No app-config, Raycast, home-directory or implicit path discovery. Refresh/connect
share one cancellation-safe async mutex; only one refresh occurs across sessions.
Unknown store keys survive refresh/revocation. Cancellation cannot undo a remote
Calendar action already accepted by Google; the host must never replay actions.

SDK contexts own listener work, HTTP futures and immutable session preferences.
Dispose/cancel/EOF/shutdown drop network futures and the loopback listener. No
startup worker, durable background lease, tray, polling timer or detached task.
Host shell/clipboard requests remain subject to user policy and manifest grants.
Static views use refreshMs:null; explicit refresh and server search work normally.

Build and verify (no dependency installation required):

```sh
cargo fmt --check
cargo clippy --locked -j1 --all-targets --all-features -- -D warnings
cargo test --locked -j1
cargo build --locked -j1 --features fixture-network
python3 scripts/protocol_test.py --binary target/debug/tezbar-google-calendar --fixture-network
cargo build --locked --release -j1
python3 scripts/package.py --target aarch64-apple-darwin --binary target/release/tezbar-google-calendar
python3 scripts/smoke.py --target aarch64-apple-darwin --archive dist/google-calendar-aarch64-apple-darwin.zip
```

Use .exe and x86_64-pc-windows-msvc on Windows. The fixture-network feature is test
only: it accepts only explicit http://127.0.0.1:PORT endpoints, redirects disabled.
Release builds/package CI use default features and cannot override Google endpoints.
The packager verifies --build-info target architecture and rejects fixture-enabled binaries.
All protocol tests use a fake host and local HTTP server, no live browser, account,
calendar, clipboard or download. Packaging produces deterministic sorted ZIPs,
complete SHA-256 file inventories and a separate artifact sidecar. Publication,
catalog pinning and real Windows/live Google validation are separate parent gates.

For a focused check of the first OAuth browser request, use the fixture build:

```sh
python3 scripts/protocol_test.py --binary target/debug/tezbar-google-calendar --fixture-network --oauth-probe
```

On Windows use the `.exe` binary. Failures include the frame type, method and
fixture error code/message, omitting OAuth URLs, token fields and view contents.
`--host-platform Windows` overrides only `initialize.platform` in this probe;
running it on macOS does not verify native Windows behavior. The full fixture
harness remains required and uses the same initial OAuth checks.

People API routing/read-mask/cache warmup follow the official endpoint contracts:
https://developers.google.com/people/api/rest/v1/people/searchContacts
and https://developers.google.com/people/api/rest/v1/otherContacts/search.
The extracted adapter's single www.googleapis.com origin and photos mask for other
contacts were corrected; these are documented port repairs, not live API validation.
