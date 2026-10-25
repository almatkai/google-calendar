# Validation and remaining limits

Recorded on 2026-10-25 using macOS 26.3 ARM64, rustc 1.98.0,
`aarch64-apple-darwin`. Local evidence does not establish Windows or macos-15
runner behavior. The native workflow is authored; the parent must run and review
both native CI targets before platform release.

SDK: immutable git revision `9f91afcc071c3f301414dc62b33cfeac7d89a0cf`.
Package schema 3, JSONL protocol 1. No app/Tauri dependency or runtime toolchain.
Source `artifacts` stays empty until reviewed artifacts are actually distributed.
ZIP and executable hashes live in adjacent `.artifact.json`; the full ZIP file
inventory is in `.inventory.json` and runtime members in `extension.json.files`.
These are local artifacts, not proof of publication, signatures or host installation.

## Local checks

The focused review passed 9 Rust tests (9 unit + 0 Rust pipe tests),
with zero failures/ignored tests, including all 5 original adapter regressions.
Locked production release, strict Clippy and formatting passed. Deterministic ZIP
comparison and isolated extracted executable smoke are required packaging checks.
To reproduce from this directory:

```sh
cargo fmt --check
cargo clippy --locked --all-targets --all-features -j1 -- -D warnings
cargo test --locked --all-features -j1 -- --test-threads=1
cargo build --locked --release -j1
```

Use the package and smoke commands in README on the matching native platform.
All fixture stores and socket listeners are temporary; no live user credentials,
provider writes, installed extension data or user listener enumeration are tested.

## Coverage and unverified integration

Rust tests include reconnect/CAS/unknown-key and malformed migration-marker
checks. Original TypeScript tests passed in the preceding implementation round
25/25 in 7 suites with existing dependencies; this focused round does not edit or
reinstall TypeScript. The actual fixture-network executable harness passed 90
assertions; the production/extracted harness passed 36 each. These fixtures cover
PKCE/state, callback fragmentation, cancellation/dispose/EOF socket closure,
concurrent single refresh, Calendar/People semantics and corruption/migration.
The package script rejects fixture-enabled binaries.

Reproduce the additional harness with a fixture-network debug build and
`scripts/protocol_test.py --binary target/debug/tezbar-google-calendar --fixture-network`.
Never package that build. No working Desktop OAuth client or secret is bundled.
Live Desktop consent/scopes/APIs, real browser/clipboard, Windows token ACLs and
clean-profile Tezbar install remain unverified. Tokens are private plain JSON
(Unix directory 0700/file 0600); Windows privacy depends on host dataDir ACLs.
Only host-provided `migrationPaths.nativeGoogleOAuth` may import the exact legacy
`native-google-oauth/tokens.json` file; no app config or Raycast token discovery.
All commands request 240000 ms; host deadline clamp must preserve that budget
for the 180-second loopback wait plus token/API requests. Long pagination or
multiple queued auth operations can still exhaust that deadline. Never replay a
write automatically: Google may have accepted it before timeout/cancellation.
