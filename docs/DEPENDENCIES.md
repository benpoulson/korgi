# Korgi -- Dependency Notes

Notes on each dependency choice, version compatibility, and known issues.

## Core Dependencies

### ssh2 0.9 (libssh2)
**Purpose:** SSH sessions, host-key verification, and the Docker socket tunnel (`src/ssh/tunnel.rs`).
**API notes:**
- `Session` serialises channel operations; the tunnel runs the session non-blocking with one pump thread per host, and `exec` shares a lock with channel opens.
- In non-blocking mode a write may return `EAGAIN` after sending part of a packet; libssh2 requires the same data to be resent before anything else.
- Do not call `Channel::flush()` on the tunnel: it discards unread incoming data.

### bollard 0.20
**Purpose:** Docker API client for all container operations.  
**Why this version:** Latest stable. Has built-in SSH support.  
**API notes (bollard 0.20 breaking changes from 0.18):**
- Query parameter types moved to `bollard::query_parameters::*` (not `bollard::container::*`)
- Container creation uses `bollard::models::ContainerCreateBody` (not `ContainerConfig`)
- `ContainerConfig` is the image-level config (read-only, from `docker inspect`)
- `ListContainersOptions.filters` is `Option<HashMap<...>>` (wrapped in Option)
- `CreateImageOptions.from_image` and `.tag` are `Option<String>` (not bare String)
- `StopContainerOptions` has `signal: Option<String>` field (must be provided)
- `StartContainerOptions` is not generic -- use `None::<StartContainerOptions>`
- Connected with `Docker::connect_with_unix()` to the tunnel's local socket.
- `Docker::ping()` returns `Result<String>` -- needs type annotation `let _: String = ...`
- `NetworkCreateRequest.name` is `String` (not `Option<String>`)
- `logs()` returns a `Stream`, not a `Future` -- do NOT `.await` it directly


### clap 4
**Purpose:** CLI argument parsing.  
**Notes:** Using derive macros. All subcommands defined in `src/cli/mod.rs`.

### tokio 1 (full features)
**Purpose:** Async runtime for bollard and the orchestrator.

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
**Note:** Currently unused in practice -- all errors go through anyhow. Will be useful when extracting library crates.

## Logging

### tracing 0.1 + tracing-subscriber 0.3
**Purpose:** Structured logging.  
**Usage:** `RUST_LOG=debug korgi deploy` for debug output.  
**Notes:** `env-filter` feature enabled for `RUST_LOG` support.

## Other

### futures 0.3
**Purpose:** `StreamExt` for consuming Docker log streams and image pull streams.

### async-trait 0.1
**Purpose:** Async methods on the Docker traits in `src/docker/traits.rs`.

---

## Dependency Graph (simplified)

```
korgi
├── clap (CLI)
├── toml + serde (config)
├── ssh2 (SSH session + Docker socket tunnel)
├── bollard (Docker API, over the tunnel's local Unix socket)
├── tokio (async runtime)
├── tracing (logging)
├── tabled + indicatif + console (output)
├── anyhow + thiserror (errors)
└── futures (stream utilities)
```

## Known Issues

### SSH host key verification
Korgi verifies remote SSH hosts against `~/.ssh/known_hosts` through the `ssh2` crate. Unknown hosts can be trusted interactively and persisted to the OpenSSH known-hosts file. Mismatches fail closed.

### tabled rendering in narrow terminals
Very wide tables (many columns, long image names) may wrap or truncate in narrow terminals. Consider `--json` output for scripting.
