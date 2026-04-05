# Deployment Pipeline

Korgi's zero-downtime deployment follows a 7-phase pipeline. The critical ordering ensures traffic is shifted to new containers **before** old ones are stopped.

## Phases

```
1. PREPARE       -- query live state, compute placement, find free ports
2. PULL          -- pull image on target hosts
3. START GREEN   -- create new containers with host port bindings
4. HEALTH CHECK  -- wait for containers to become healthy
   |-- failure   -- stop & remove new containers, abort (old gen untouched)
5. SYNC CONFIG   -- update Traefik routing to point to new containers
6. DRAIN OLD     -- gracefully stop ALL old generation containers
7. CLEANUP       -- remove containers beyond rollback_keep
```

The old generation is **never stopped** until Traefik has been updated to route traffic to the new containers (phase 5). This is what makes the deployment truly zero-downtime.

## Phase details

### 1. PREPARE

- Loads config, connects to Docker on all hosts via SSH
- Queries live state (containers with `korgi.*` labels)
- Computes placement (round-robin across hosts matching `placement_labels`)
- Determines next generation number (`max(existing) + 1`)

### 2. PULL

- Pulls the image on each target host
- Uses registry credentials if configured (GHCR shorthand or explicit)
- Aborts if pull fails on any host

### 3. START GREEN

- Ensures the Docker network exists on target hosts
- Creates new containers with:
  - Korgi metadata labels (`korgi.project`, `korgi.service`, `korgi.generation`, `korgi.instance`)
  - Traefik routing labels (if service has `[routing]`)
  - Host port bindings (if `host_base` or `host` configured)
  - Docker HEALTHCHECK (if health mode is `docker`)

### 4. HEALTH CHECK

- **Docker mode**: Polls `docker inspect` until `.State.Health.Status == "healthy"`
- **HTTP mode**: Makes HTTP requests to `internal_address:host_port/health_path`
- Timeout: `drain_seconds * 2`
- On failure: stops and removes ALL new containers, old generation keeps serving

### 5. SYNC CONFIG

- Regenerates Traefik file-provider YAML with the new container topology
- Writes it into the Traefik container on all LB hosts
- **Traffic now routes to new containers** -- old containers still running but receiving no new requests

### 6. DRAIN OLD

- Stops ALL running containers from previous generations (not just N-1)
- Sends `docker stop -t {drain_seconds}` -- SIGTERM, wait, then SIGKILL
- Safe because Traefik was already updated in phase 5

### 7. CLEANUP

- Removes containers from generations older than `current - rollback_keep`
- Force-stops any still-running containers beyond the keep threshold
- Recent stopped generations are kept for rollback

## Port allocation

When using `host_base` ports, each generation gets a unique port range to avoid collisions during deployment:

- Generation 1: `host_base + 0`, `host_base + 1`, ...
- Generation 2: `host_base + replicas`, `host_base + replicas + 1`, ...

If the calculated ports are already occupied (by another service or leftover containers), korgi automatically finds the next free range.

## Error boundaries

The critical invariant: **the old generation is never stopped until Traefik has been updated to route traffic to the new containers.**

| Phase | Failure | State |
|-------|---------|-------|
| PULL | Image not found | No changes made |
| START GREEN | Container creation fails | Clean up new containers, old gen serves |
| HEALTH CHECK | Timeout or unhealthy | Remove new containers, old gen serves |
| DRAIN OLD | Stop fails | New gen already serving, recoverable |
| CLEANUP | Remove fails | Stale containers remain (harmless) |

## Rollback

`korgi rollback --service <name>` finds the most recent stopped generation and:

1. Verifies the image exists (re-pulls if garbage-collected)
2. Starts the old containers (labels still intact)
3. Stops the current generation
4. Syncs Traefik config

## Dry run

`korgi deploy --dry-run` shows what would happen without making changes:

```
$ korgi deploy --dry-run
Dry run -- would deploy:
  korgi-myapp-api-g5-0 on worker-1
  korgi-myapp-api-g5-1 on worker-2
```

## Container naming

```
korgi-{project}-{service}-g{generation}-{instance}
```

Examples: `korgi-myapp-api-g4-0`, `korgi-myapp-api-g4-1`

## Generation lifecycle

```
Generation 1: Created -> Running -> Stopped (drained by gen 2)
Generation 2: Created -> Running -> Stopped (drained by gen 3)
Generation 3: Created -> Running (current)
```
