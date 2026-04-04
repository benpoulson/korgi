# Korgi — Dependency Notes

Notes on each dependency choice, version compatibility, and known issues.

## Core Dependencies

### russh 0.48 + russh-keys 0.48
**Purpose:** SSH connections for `korgi check` and `korgi exec`.  
**Why this version:** Latest stable. Pure Rust, async/tokio-native, no C dependencies.  
**API notes:**
- `client::Handler` requires `#[async_trait]` — the trait uses async methods with lifetime constraints
- `check_server_key` receives `ssh_key::PublicKey`, not russh's own type (re-exported from `ssh-key` crate)
- `authenticate_publickey` takes `Arc<PrivateKey>`, loaded via `russh_keys::load_secret_key`
- No built-in SSH agent support via method name `authenticate_publickey_with_agent` — must use explicit key path
- `channel.wait()` returns `Option<ChannelMsg>` — `None` means channel closed

**Alternatives considered:** `thrussh` (unmaintained predecessor), `ssh2-rs` (C bindings to libssh2).

### bollard 0.20 (with `ssh` feature)
**Purpose:** Docker API client for all container operations.  
**Why this version:** Latest stable. Has built-in SSH support.  
**API notes (bollard 0.20 breaking changes from 0.18):**
- Query parameter types moved to `bollard::query_parameters::*` (not `bollard::container::*`)
- Container creation uses `bollard::models::ContainerCreateBody` (not `ContainerConfig`)
- `ContainerConfig` is the image-level config (read-only, from `docker inspect`)
- `ListContainersOptions.filters` is `Option<HashMap<...>>` (wrapped in Option)
- `CreateImageOptions.from_image` and `.tag` are `Option<String>` (not bare String)
- `StopContainerOptions` has `signal: Option<String>` field (must be provided)
- `StartContainerOptions` is not generic — use `None::<StartContainerOptions>`
- `Docker::connect_with_ssh()` signature: `(addr, timeout_secs, api_version, keypair_path)`
- `Docker::ping()` returns `Result<String>` — needs type annotation `let _: String = ...`
- `NetworkCreateRequest.name` is `String` (not `Option<String>`)
- `logs()` returns a `Stream`, not a `Future` — do NOT `.await` it directly

**SSH feature:** Enabled via `features = ["ssh"]` in Cargo.toml. Uses the `openssh` crate internally to spawn SSH subprocesses. Requires the system SSH binary.

### clap 4
**Purpose:** CLI argument parsing.  
**Notes:** Using derive macros. All subcommands defined in `src/cli/mod.rs`.

### tokio 1 (full features)
**Purpose:** Async runtime. Required by both russh and bollard.

### serde 1 + toml 0.8 + serde_json 1
**Purpose:** Config parsing and JSON output.  
**Notes:** `serde(default)` used extensively for optional config fields.

## Output Dependencies

### tabled 0.17
**Purpose:** ASCII tables for `korgi status`.  
**Notes:** Uses `#[derive(Tabled)]` for row types.

### indicatif 0.17
**Purpose:** Progress bars and spinners for deploy operations.

### console 0.15
**Purpose:** Terminal colors and styling (`style("text").green().bold()`).

## Error Handling

### anyhow 1
**Purpose:** Error propagation in CLI commands and main.  
**Usage:** `anyhow::Result<()>` as return type, `context()` for error messages.

### thiserror 2
**Purpose:** Typed errors for library code.  
**Note:** Currently unused in practice — all errors go through anyhow. Will be useful when extracting library crates.

## Logging

### tracing 0.1 + tracing-subscriber 0.3
**Purpose:** Structured logging.  
**Usage:** `RUST_LOG=debug korgi deploy` for debug output.  
**Notes:** `env-filter` feature enabled for `RUST_LOG` support.

## Other

### futures 0.3
**Purpose:** `StreamExt` for consuming Docker log streams and image pull streams.

### ssh-key 0.6
**Purpose:** `PublicKey` type required by russh's `check_server_key` handler. Re-exported.

### async-trait 0.1
**Purpose:** Required for implementing `russh::client::Handler` trait (has async methods with lifetime bounds).

---

## Dependency Graph (simplified)

```
korgi
├── clap (CLI)
├── toml + serde (config)
├── russh + russh-keys + ssh-key + async-trait (SSH)
├── bollard (Docker API)
│   └── openssh (SSH transport, via "ssh" feature)
│       └── spawns system ssh binary
├── tokio (async runtime)
├── tracing (logging)
├── tabled + indicatif + console (output)
├── anyhow + thiserror (errors)
└── futures (stream utilities)
```

## Known Issues

### bollard SSH on systems without OpenSSH
The `openssh` crate requires a system SSH binary (`ssh` command). On minimal Docker images or Windows without OpenSSH, this will fail. Korgi is primarily designed for Linux/macOS.

### russh host key verification
Currently `check_server_key` always returns `true` (accepts any host key). For production use, this should verify against `~/.ssh/known_hosts`. The russh API provides the server's public key but korgi must implement the checking logic.

### tabled rendering in narrow terminals
Very wide tables (many columns, long image names) may wrap or truncate in narrow terminals. Consider `--json` output for scripting.
