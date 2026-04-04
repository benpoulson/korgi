# Kargo Architecture Plan

## 1. TOML Configuration Schema

### `kargo.toml` (project root)

```toml
[project]
name = "myapp"
# Optional: default environment
default_env = "production"

# ─── Host Definitions ───────────────────────────────────────

[[hosts]]
name = "web1"
address = "10.0.1.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
port = 22                          # optional, default 22
labels = ["web", "us-east"]        # for placement constraints
docker_socket = "/var/run/docker.sock"  # optional, default shown

[[hosts]]
name = "web2"
address = "10.0.1.11"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web", "us-east"]

[[hosts]]
name = "web3"
address = "10.0.1.12"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web", "us-west"]

# ─── Traefik Configuration ──────────────────────────────────

[traefik]
image = "traefik:v3.1"
# Which hosts run Traefik (by name or label selector)
hosts = ["web1", "web2", "web3"]
dashboard = false
log_level = "INFO"

  [traefik.entrypoints]
  web = { address = ":80" }
  websecure = { address = ":443" }

  # Optional: Let's Encrypt
  [traefik.acme]
  email = "ops@example.com"
  storage = "/letsencrypt/acme.json"
  http_challenge_entrypoint = "web"

  # Bind-mount for ACME persistence
  [[traefik.volumes]]
  source = "/opt/traefik/letsencrypt"
  target = "/letsencrypt"

# ─── Service Definitions ────────────────────────────────────

[[services]]
name = "api"
image = "registry.example.com/myapp/api:latest"
replicas = 3
# Placement: spread across hosts matching these labels
placement_labels = ["web"]
# Or pin to specific hosts:
# placement_hosts = ["web1", "web2"]

  [services.env]
  DATABASE_URL = "postgres://db:5432/myapp"
  REDIS_URL = "redis://cache:6379"

  # Environment variable references (from .env or env-specific files)
  # SECRET_KEY = "${SECRET_KEY}"

  [[services.ports]]
  container = 8080
  # host port is auto-assigned; Traefik routes by label

  [services.health_check]
  path = "/healthz"
  interval = "10s"
  timeout = "5s"
  retries = 3
  start_period = "15s"

  [services.routing]
  rule = "Host(`api.example.com`)"
  entrypoints = ["websecure"]
  tls = true
  # Optional: path-based routing
  # rule = "Host(`example.com`) && PathPrefix(`/api`)"

  [services.resources]
  memory = "512m"
  cpus = "1.0"

  [services.deploy]
  # Drain time before stopping old containers
  drain_seconds = 30
  # Keep N old containers stopped for rollback
  rollback_keep = 1

[[services]]
name = "worker"
image = "registry.example.com/myapp/worker:latest"
replicas = 2
placement_labels = ["web"]
# No routing block = no Traefik labels (background worker)

  [services.env]
  DATABASE_URL = "postgres://db:5432/myapp"
  QUEUE_NAME = "default"

  [services.health_check]
  # TCP health check (no path = TCP check on first exposed port)
  interval = "15s"
  timeout = "5s"
  retries = 3

# ─── Environment Overrides ──────────────────────────────────
# Optional: kargo.staging.toml overrides specific fields
# Loaded via: kargo deploy --env staging
```

### `kargo.staging.toml` (environment overlay)

```toml
# Only fields that differ; deep-merged over kargo.toml

[[hosts]]
name = "staging1"
address = "10.0.2.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["web"]

[traefik]
hosts = ["staging1"]

[[services]]
name = "api"
image = "registry.example.com/myapp/api:staging"
replicas = 1
placement_labels = ["web"]

  [services.routing]
  rule = "Host(`api.staging.example.com`)"
```

### Config loading strategy

Base config (`kargo.toml`) is loaded first. If `--env <name>` is passed, `kargo.<name>.toml` is deep-merged on top. Environment variables can be interpolated with `${VAR}` syntax, resolved at runtime from the process environment or a `.env` file.

---

## 2. CLI Command Structure

```
kargo
├── init                          # Scaffold a new kargo.toml
├── check                         # Validate config, test SSH to all hosts
│   └── --env <name>
├── status                        # Show running services across all hosts
│   ├── --env <name>
│   ├── --service <name>          # Filter to one service
│   └── --json                    # Machine-readable output
├── deploy                        # Deploy all (or specific) services
│   ├── --env <name>
│   ├── --service <name>          # Deploy just one service
│   ├── --image <image>           # Override image (for CI: kargo deploy --service api --image api:sha-abc123)
│   ├── --hosts <h1,h2>           # Limit to specific hosts
│   └── --dry-run                 # Show what would happen
├── rollback                      # Roll back to previous container
│   ├── --env <name>
│   └── --service <name>          # Required
├── scale                         # Change replica count
│   ├── --env <name>
│   ├── --service <name>          # Required
│   └── <count>                   # New replica count
├── traefik                       # Traefik sub-commands
│   ├── deploy                    # Deploy/update Traefik on configured hosts
│   │   └── --env <name>
│   ├── status                    # Show Traefik container status
│   └── logs <host>               # Tail Traefik logs on a host
├── exec                          # Run a one-off command in a service container
│   ├── --env <name>
│   ├── --service <name>          # Required
│   ├── --host <name>             # Which host (default: first available)
│   └── <command...>
├── logs                          # Tail logs for a service
│   ├── --env <name>
│   ├── --service <name>
│   ├── --host <name>
│   ├── --tail <n>
│   └── --follow
└── destroy                       # Stop and remove service containers
    ├── --env <name>
    ├── --service <name>
    └── --yes                     # Skip confirmation
```

Clap derive structure:

```rust
#[derive(Parser)]
#[command(name = "kargo", about = "Container orchestration over SSH")]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Config file path (default: ./kargo.toml)
    #[arg(long, global = true, default_value = "kargo.toml")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    Init,
    Check { #[arg(long)] env: Option<String> },
    Status { #[arg(long)] env: Option<String>, #[arg(long)] service: Option<String>, #[arg(long)] json: bool },
    Deploy(DeployArgs),
    Rollback { #[arg(long)] env: Option<String>, #[arg(long)] service: String },
    Scale { #[arg(long)] env: Option<String>, #[arg(long)] service: String, count: u32 },
    Traefik { #[command(subcommand)] command: TraefikCommand },
    Exec { #[arg(long)] env: Option<String>, #[arg(long)] service: String, #[arg(long)] host: Option<String>, command: Vec<String> },
    Logs { #[arg(long)] env: Option<String>, #[arg(long)] service: Option<String>, #[arg(long)] host: Option<String>, #[arg(long)] tail: Option<u32>, #[arg(long)] follow: bool },
    Destroy { #[arg(long)] env: Option<String>, #[arg(long)] service: Option<String>, #[arg(long)] yes: bool },
}
```

---

## 3. Rust Module / Crate Layout

Single crate (binary), split into a `lib.rs` + `main.rs` pattern for testability.

```
kargo/
├── Cargo.toml
├── src/
│   ├── main.rs                    # Entry point: parse CLI, load config, dispatch
│   ├── lib.rs                     # Re-export public modules
│   │
│   ├── cli/
│   │   ├── mod.rs                 # Clap definitions (Cli, Command, subcommands)
│   │   └── output.rs              # Table formatting, JSON output, progress bars
│   │
│   ├── config/
│   │   ├── mod.rs                 # Top-level config loading, merging, validation
│   │   ├── types.rs               # Serde structs: ProjectConfig, HostConfig, ServiceConfig, etc.
│   │   ├── merge.rs               # Deep-merge logic for environment overlays
│   │   └── interpolate.rs         # ${VAR} expansion from env/dotenv
│   │
│   ├── ssh/
│   │   ├── mod.rs                 # SshPool: manages connections to all hosts
│   │   ├── session.rs             # SshSession: wraps russh Handle, exec commands, stream docker
│   │   └── transport.rs           # DockerSshTransport: bridges russh channel to bollard
│   │
│   ├── docker/
│   │   ├── mod.rs                 # DockerHost: bollard client bound to a specific host via SSH
│   │   ├── containers.rs          # Create, start, stop, remove, list, inspect containers
│   │   ├── images.rs              # Pull images
│   │   └── labels.rs              # Generate Traefik Docker labels from ServiceConfig
│   │
│   ├── orchestrator/
│   │   ├── mod.rs                 # Top-level orchestration: coordinates deployment across hosts
│   │   ├── placement.rs           # Decide which hosts get which replicas
│   │   ├── deploy.rs              # Zero-downtime deployment pipeline
│   │   ├── rollback.rs            # Rollback logic
│   │   ├── scale.rs               # Scale up/down logic
│   │   └── state.rs               # Live state querying: what's actually running
│   │
│   ├── health/
│   │   ├── mod.rs                 # Health check orchestration
│   │   └── checker.rs             # HTTP/TCP health check implementations
│   │
│   ├── traefik/
│   │   ├── mod.rs                 # Traefik deployment and management
│   │   └── config.rs              # Generate Traefik static config, Docker labels
│   │
│   ├── commands/
│   │   ├── mod.rs
│   │   ├── init.rs                # `kargo init` handler
│   │   ├── check.rs               # `kargo check` handler
│   │   ├── status.rs              # `kargo status` handler
│   │   ├── deploy.rs              # `kargo deploy` handler
│   │   ├── rollback.rs            # `kargo rollback` handler
│   │   ├── scale.rs               # `kargo scale` handler
│   │   ├── traefik.rs             # `kargo traefik *` handlers
│   │   ├── exec.rs                # `kargo exec` handler
│   │   ├── logs.rs                # `kargo logs` handler
│   │   └── destroy.rs             # `kargo destroy` handler
│   │
│   └── error.rs                   # Unified error type (thiserror)
│
└── tests/
    ├── config_test.rs             # Config loading, merging, validation
    ├── labels_test.rs             # Traefik label generation
    ├── placement_test.rs          # Placement algorithm
    └── integration/               # Integration tests (require Docker + SSH)
```

### Key dependencies in `Cargo.toml`

```toml
[dependencies]
clap = { version = "4", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
toml = "0.8"
russh = "0.46"
russh-keys = "0.46"
bollard = "0.18"
hyper = "1"
hyper-util = "0.1"
http-body-util = "0.1"
reqwest = { version = "0.12", features = ["rustls-tls"], default-features = false }
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
indicatif = "0.17"       # Progress bars
comfy-table = "7"         # Status tables
dotenvy = "0.15"          # .env loading
serde_json = "1"
futures = "0.3"
pin-project-lite = "0.2"  # For custom transport
```

---

## 4. Core Abstractions and Traits

### SSH Layer

```rust
// ssh/session.rs

/// A connected SSH session to a single host.
/// Wraps a russh client Handle and provides high-level operations.
pub struct SshSession {
    handle: russh::client::Handle<SshHandler>,
    host_config: HostConfig,
}

impl SshSession {
    /// Connect to a host using its config.
    pub async fn connect(host: &HostConfig) -> Result<Self>;

    /// Execute a command, return (exit_code, stdout, stderr).
    pub async fn exec(&self, command: &str) -> Result<ExecOutput>;

    /// Open a channel to the remote Docker Unix socket.
    /// Returns an AsyncRead+AsyncWrite stream connected to /var/run/docker.sock.
    pub async fn docker_stream(&self) -> Result<ChannelStream<Msg>>;
}
```

```rust
// ssh/mod.rs

/// Pool of SSH connections, one per host. Connections are established
/// lazily and reused. If a connection drops, it reconnects on next use.
pub struct SshPool {
    sessions: HashMap<String, SshSession>,  // keyed by host name
}

impl SshPool {
    pub async fn new(hosts: &[HostConfig]) -> Result<Self>;
    pub async fn get(&self, host_name: &str) -> Result<&SshSession>;
    pub async fn check_all(&self) -> Vec<(String, Result<()>)>;
}
```

### Docker-over-SSH Transport

```rust
// ssh/transport.rs

/// Custom bollard transport that sends HTTP requests over an SSH channel
/// connected to the remote Docker Unix socket via streamlocal forwarding.
///
/// For each HTTP request, we open a new streamlocal channel to the remote
/// docker.sock, write the HTTP request bytes, and read the response.
///
/// This avoids the need for TCP port forwarding or socat on the remote host.
pub struct DockerSshTransport {
    ssh_session: Arc<SshSession>,
    docker_socket: String,  // e.g. "/var/run/docker.sock"
}

impl DockerSshTransport {
    pub fn new(session: Arc<SshSession>, socket_path: &str) -> Self;
}

// Implements the closure signature expected by bollard's
// Docker::connect_with_custom_transport:
//   Fn(Request<Body>) -> Future<Output = Result<Response<Incoming>>>
```

```rust
// docker/mod.rs

/// A Docker client bound to a specific remote host via SSH.
pub struct DockerHost {
    pub host_name: String,
    client: bollard::Docker,    // connected via DockerSshTransport
    session: Arc<SshSession>,
}

impl DockerHost {
    pub async fn new(session: Arc<SshSession>, host: &HostConfig) -> Result<Self>;

    // Delegate to bollard with typed wrappers:
    pub async fn pull_image(&self, image: &str) -> Result<()>;
    pub async fn create_container(&self, opts: &ContainerSpec) -> Result<String>;
    pub async fn start_container(&self, id: &str) -> Result<()>;
    pub async fn stop_container(&self, id: &str, timeout: Duration) -> Result<()>;
    pub async fn remove_container(&self, id: &str) -> Result<()>;
    pub async fn list_containers(&self, filters: ContainerFilters) -> Result<Vec<ContainerInfo>>;
    pub async fn inspect_container(&self, id: &str) -> Result<ContainerDetail>;
    pub async fn container_logs(&self, id: &str, opts: LogOpts) -> Result<impl Stream>;
}
```

### Orchestration

```rust
// orchestrator/placement.rs

/// Decides which hosts receive which replicas of a service.
/// Strategy: spread evenly across eligible hosts (matching placement labels/names).
pub struct PlacementPlan {
    /// host_name -> number of replicas to run on that host
    pub assignments: HashMap<String, u32>,
}

pub fn compute_placement(
    service: &ServiceConfig,
    hosts: &[HostConfig],
    current_state: &LiveState,
) -> Result<PlacementPlan>;
```

```rust
// orchestrator/state.rs

/// Queries all hosts to build a picture of what's actually running.
/// This is the ONLY source of truth — no local state file.
///
/// Containers are identified as kargo-managed by Docker labels:
///   kargo.service = "api"
///   kargo.project = "myapp"
///   kargo.generation = "3"  (monotonically increasing per deploy)
pub struct LiveState {
    /// host_name -> list of managed containers
    pub containers: HashMap<String, Vec<ManagedContainer>>,
}

pub struct ManagedContainer {
    pub id: String,
    pub service: String,
    pub project: String,
    pub generation: u64,
    pub image: String,
    pub status: ContainerStatus,
    pub health: Option<HealthStatus>,
    pub created_at: DateTime<Utc>,
    pub labels: HashMap<String, String>,
}

impl LiveState {
    /// Query all hosts in parallel and return current state.
    pub async fn query(
        docker_hosts: &HashMap<String, DockerHost>,
        project: &str,
    ) -> Result<Self>;

    pub fn service_containers(&self, service: &str) -> Vec<(&str, &ManagedContainer)>;
    pub fn current_generation(&self, service: &str) -> u64;
}
```

### Health Checking

```rust
// health/checker.rs

/// Performs health checks against a container.
/// For containers with HTTP health checks, makes requests via SSH tunnel.
/// Also checks Docker's built-in HEALTHCHECK status.
pub struct HealthChecker {
    docker_host: Arc<DockerHost>,
}

impl HealthChecker {
    /// Poll container health until healthy or timeout.
    /// Returns Ok(()) if healthy, Err if timeout/failed.
    pub async fn wait_healthy(
        &self,
        container_id: &str,
        config: &HealthCheckConfig,
        timeout: Duration,
    ) -> Result<()>;
}
```

### Label Generation

```rust
// docker/labels.rs

/// Generates all Docker labels for a container, including:
/// - Kargo management labels (kargo.service, kargo.project, kargo.generation)
/// - Traefik routing labels (if service has routing config)
pub fn generate_labels(
    project: &str,
    service: &ServiceConfig,
    generation: u64,
    instance_index: u32,
    active: bool,  // whether Traefik should route to this container
) -> HashMap<String, String>;

/// Generate labels that disable Traefik routing (for draining).
pub fn drain_labels(
    project: &str,
    service: &ServiceConfig,
    generation: u64,
    instance_index: u32,
) -> HashMap<String, String>;
```

---

## 5. Deployment Pipeline (step by step)

### `kargo deploy --service api`

```
┌──────────────────────────────────────────────────────┐
│  PHASE 0: PREPARATION                                │
├──────────────────────────────────────────────────────┤
│ 1. Load & validate config (merge env overlay)        │
│ 2. Establish SSH connections to all relevant hosts    │
│ 3. Query live state from all hosts (in parallel)     │
│ 4. Compute placement plan                            │
│ 5. Calculate next generation number (current + 1)    │
│ 6. If --dry-run: print plan and exit                 │
└──────────────────┬───────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────┐
│  PHASE 1: IMAGE PULL (parallel across hosts)         │
├──────────────────────────────────────────────────────┤
│ For each host in placement plan (concurrently):      │
│   docker.pull_image("registry/api:v2")               │
│                                                      │
│ On failure: abort deploy, report which hosts failed  │
└──────────────────┬───────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────┐
│  PHASE 2: START GREEN CONTAINERS (parallel/hosts)    │
├──────────────────────────────────────────────────────┤
│ For each host, for each replica assigned:            │
│   1. Generate container name:                        │
│      kargo-{project}-{service}-{generation}-{index}  │
│   2. Generate labels:                                │
│      - kargo.service = "api"                         │
│      - kargo.project = "myapp"                       │
│      - kargo.generation = "4"                        │
│      - traefik.enable = "true"                       │
│      - traefik.http.routers.api-g4-i0.rule = ...     │
│      - traefik.http.routers.api-g4-i0.priority = 1   │
│        (LOW priority so Traefik still prefers old)   │
│      - traefik.http.services.api-g4-i0....port = ... │
│   3. Create container with Docker HEALTHCHECK        │
│   4. Start container                                 │
│                                                      │
│ On failure: stop+remove any green containers started │
│             in this deploy, abort                     │
└──────────────────┬───────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────┐
│  PHASE 3: HEALTH CHECK GREEN (parallel)              │
├──────────────────────────────────────────────────────┤
│ For each green container (concurrently):             │
│   Poll Docker inspect for Health.Status == "healthy" │
│   Timeout: start_period + (interval * retries)       │
│                                                      │
│ On failure (any container unhealthy):                │
│   1. Stop + remove ALL green containers              │
│   2. Report which containers failed health check     │
│   3. Abort deploy                                    │
└──────────────────┬───────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────┐
│  PHASE 4: TRAFFIC SWITCH                             │
├──────────────────────────────────────────────────────┤
│ Two sub-steps (can't be truly atomic across hosts,   │
│ but ordering minimizes risk):                        │
│                                                      │
│ 4a. PROMOTE GREEN: For each green container:         │
│     Update labels (via stop+recreate or label API):  │
│       traefik.http.routers.api-g4-i0.priority = 100  │
│     Traefik picks up change within ~2s (poll)        │
│                                                      │
│ 4b. DEMOTE BLUE: For each old-gen container:         │
│     Update labels:                                   │
│       traefik.enable = "false"                       │
│     Traefik stops routing to old within ~2s          │
│                                                      │
│ NOTE: Since Docker labels are immutable after create,│
│ the actual mechanism is:                             │
│   - Green containers are created with HIGH priority  │
│     from the start (priority=100), and old containers│
│     already have priority=100 too. This means during │
│     health check, BOTH old and new receive traffic   │
│     (which is fine — both are healthy).              │
│   - After health checks pass, we disable old:        │
│     stop old, recreate with traefik.enable=false,    │
│     OR just stop old (simpler).                      │
│                                                      │
│ REVISED APPROACH (simpler, leveraging Traefik):      │
│   - Green containers get traefik.enable=true from    │
│     the start. Traefik load-balances across both     │
│     old AND new during health check phase.           │
│   - After health checks pass, proceed to drain old.  │
│   - This is safe because Docker HEALTHCHECK must     │
│     pass before Traefik considers the container      │
│     healthy (if Traefik healthcheck is configured).  │
└──────────────────┬───────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────┐
│  PHASE 5: DRAIN OLD CONTAINERS                       │
├──────────────────────────────────────────────────────┤
│ For each old-generation container:                   │
│   1. Stop container with timeout = drain_seconds     │
│      (Docker sends SIGTERM, waits, then SIGKILL)     │
│   2. If rollback_keep > 0:                           │
│        Keep container stopped (don't remove)         │
│      Else:                                           │
│        Remove container                              │
│                                                      │
│ On failure: warn but don't abort (new is running)    │
└──────────────────┬───────────────────────────────────┘
                   │
┌──────────────────▼───────────────────────────────────┐
│  PHASE 6: CLEANUP                                    │
├──────────────────────────────────────────────────────┤
│ Remove containers from generations older than        │
│ (current - rollback_keep). E.g., if rollback_keep=1  │
│ and we just deployed gen 4, remove gen 1 and 2       │
│ containers (keep gen 3 stopped for rollback).        │
│                                                      │
│ Print deployment summary table.                      │
└──────────────────────────────────────────────────────┘
```

### Revised Traefik label strategy (accounting for label immutability)

Docker container labels are immutable after creation. This simplifies the design:

1. **Green containers are created with `traefik.enable=true`** and full routing labels, using a **service name that includes the generation** (e.g., `api-g4`). Traefik sees both old (`api-g3`) and new (`api-g4`) as backends for the same router.

2. **Single Traefik router, multiple services**: Use a shared router name across generations, with the weighted round-robin or sticky sessions handled by Traefik:

```
# Old container (gen 3):
traefik.http.routers.myapp-api.rule = Host(`api.example.com`)
traefik.http.services.myapp-api-g3-i0.loadbalancer.server.port = 8080

# New container (gen 4):  
traefik.http.routers.myapp-api.rule = Host(`api.example.com`)
traefik.http.services.myapp-api-g4-i0.loadbalancer.server.port = 8080
```

3. **After health checks pass**: Stop old containers. Traefik automatically removes stopped containers from its backend pool. The drain period is handled by Docker's stop timeout (SIGTERM + grace period).

4. **For rollback**: Start the stopped gen-3 containers. Their labels are still intact. Traefik rediscovers them.

This means the zero-downtime flow becomes:
- Start green (Traefik auto-adds to pool once Docker HEALTHCHECK passes)
- Wait for green health checks
- Stop old (Traefik auto-removes from pool; Docker stop timeout = drain period)

No label mutation needed at all.

### Rollback flow

```
kargo rollback --service api

1. Query live state
2. Find most recent stopped generation for this service
3. Start those stopped containers (labels still intact)
4. Wait for health checks
5. Stop current generation containers
```

### Error handling principles

- Every phase checks results before proceeding to the next.
- On failure during green startup or health check: clean up all green containers, leave blue running untouched.
- On failure during drain: warn but continue (new containers are healthy and receiving traffic).
- All operations are idempotent: re-running `kargo deploy` with the same image is safe (detects no change needed, or completes a partial deploy).

---

## 6. SSH and Docker Interaction — Technical Design

### Connection flow

```
┌─────────┐     SSH (russh)     ┌──────────┐
│  kargo  │ ◄─────────────────► │  Host    │
│  (local)│                     │  (remote)│
└────┬────┘                     └────┬─────┘
     │                               │
     │  channel_open_direct_         │
     │  streamlocal(                 │
     │    "/var/run/docker.sock"     │
     │  )                            │
     │                               │
     │  ◄── SSH Channel ──►          │
     │  (AsyncRead+AsyncWrite)       │
     │                               │
     ▼                               ▼
┌──────────┐  HTTP-over-channel  ┌──────────┐
│  bollard  │ ◄────────────────► │  dockerd │
│  client   │  (Docker Engine API│          │
└──────────┘   JSON over HTTP)   └──────────┘
```

### The `DockerSshTransport` implementation

Bollard's `Docker::connect_with_custom_transport` accepts something that implements:
```rust
Fn(BollardRequest) -> Future<Output = Result<Response<Incoming>>>
```

Our implementation:

```rust
// Pseudocode for the transport closure:

async fn docker_request(
    ssh_session: &SshSession,
    socket_path: &str,
    request: hyper::Request<Body>,
) -> Result<hyper::Response<Incoming>> {
    // 1. Open a new streamlocal channel for this request
    let channel = ssh_session
        .handle
        .channel_open_direct_streamlocal(socket_path)
        .await?;

    // 2. Convert the channel into an AsyncRead+AsyncWrite stream
    let stream = channel.into_stream();

    // 3. Use hyper's client connection to send HTTP/1.1 over the stream
    let (mut sender, conn) = hyper::client::conn::http1::handshake(
        TokioIo::new(stream)
    ).await?;

    // 4. Spawn the connection driver
    tokio::spawn(conn);

    // 5. Send the request and get the response
    let response = sender.send_request(request).await?;

    Ok(response)
}
```

Key insight: We use `channel_open_direct_streamlocal` to connect to the remote Docker Unix socket. This is the SSH equivalent of `ssh -L /tmp/docker.sock:/var/run/docker.sock host` but without needing a local socket file. Each bollard HTTP request opens a fresh channel (cheap over an existing SSH connection) or we can keep a persistent HTTP/1.1 connection over a single channel.

### Connection pooling strategy

```
SshPool
├── web1: SshSession (single SSH connection, multiplexed channels)
├── web2: SshSession
└── web3: SshSession

Each SshSession holds one russh Handle.
SSH multiplexes many channels over one TCP connection.
Each DockerHost holds an Arc<SshSession> and a bollard::Docker client.
```

For 2-10 hosts, one SSH connection per host is sufficient. SSH multiplexing handles concurrent Docker API calls naturally. No TCP port allocation or socat needed on remote hosts.

### Command execution (for non-Docker operations)

```rust
impl SshSession {
    pub async fn exec(&self, command: &str) -> Result<ExecOutput> {
        let channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = None;

        loop {
            match channel.wait().await {
                Some(ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
                Some(ChannelMsg::ExtendedData { data, ext }) if ext == 1 => {
                    stderr.extend_from_slice(&data);
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    exit_code = Some(exit_status);
                }
                Some(ChannelMsg::Eof | ChannelMsg::Close) | None => break,
                _ => {}
            }
        }

        Ok(ExecOutput {
            exit_code: exit_code.unwrap_or(255),
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
        })
    }
}
```

This is only needed for `kargo check` (testing SSH connectivity, checking Docker is running) and `kargo exec`. All container management goes through bollard over the streamlocal transport.

---

## 7. State Management Decision

**No local state file.** The live Docker containers ARE the state. Every kargo command queries the actual containers on all hosts via `LiveState::query()`.

Containers are identified as kargo-managed by labels:
```
kargo.project = "myapp"
kargo.service = "api"
kargo.generation = "4"
kargo.instance = "0"
```

`LiveState::query()` runs `docker ps -a --filter label=kargo.project=myapp` on each host in parallel, taking ~200ms over SSH. For 10 hosts, this is well under 1 second with concurrent queries.

Benefits:
- No state drift between what kargo thinks is running and what is actually running
- No state file to lose, corrupt, or conflict across team members
- Works identically in CI and local usage
- `kargo status` always shows ground truth

The generation number (monotonically increasing per service) is determined by querying the max generation from live containers and incrementing. If no containers exist, generation starts at 1.

---

## 8. Naming Conventions

Container names follow this pattern:
```
kargo-{project}-{service}-g{generation}-{instance_index}
```
Examples:
```
kargo-myapp-api-g4-0
kargo-myapp-api-g4-1
kargo-myapp-api-g4-2
kargo-myapp-worker-g7-0
kargo-myapp-traefik-0        (traefik has no generation)
```

Docker network (one per host, created if not exists):
```
kargo-{project}
```

---

## 9. Implementation Order

1. **Config module** — Parse TOML, validate, merge environments. Pure logic, fully testable without SSH.
2. **CLI skeleton** — Clap definitions, dispatch to stubbed handlers.
3. **SSH module** — Connect, exec commands, `kargo check` works.
4. **Docker-over-SSH transport** — The hard part. Get bollard talking over streamlocal. Test with `docker ps`.
5. **Live state querying** — `kargo status` works.
6. **Traefik deployment** — `kargo traefik deploy` creates Traefik containers.
7. **Label generation** — Unit-testable module.
8. **Deployment pipeline** — `kargo deploy` with zero-downtime.
9. **Health checking** — Integrated into deploy pipeline.
10. **Scale, rollback, logs, exec** — Straightforward once the core is solid.
11. **Polish** — Progress bars, better error messages, `--dry-run`, `--json`.
