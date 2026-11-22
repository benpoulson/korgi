# Deployment Pipeline

Korgi's zero-downtime deployment follows an 8-phase pipeline. The critical ordering ensures traffic is shifted to new containers **before** old ones are stopped.

## Phases

```
1. PREPARE       -- query live state, compute placement, find free ports
2. PULL          -- pull image on target hosts
3. START GREEN   -- create new containers with host port bindings
4. HEALTH CHECK  -- wait for all new containers (every selected service) to be healthy
   |-- failure   -- remove ALL new containers, abort (old gens untouched)
5. SYNC CONFIG   -- update Traefik on EVERY lb host, read the file back to confirm
   |-- failure   -- abort without draining (old and new both keep running)
6. DRAIN OLD     -- gracefully stop ALL old generation containers (concurrently)
7. SYNC CONFIG   -- remove drained containers from routing (always attempted)
8. CLEANUP       -- remove generations beyond rollback_keep
```

The old generation is **never stopped** unless the new containers are verified healthy **and** every Traefik host has confirmed the new routing config (phase 5). This is what makes the deployment truly zero-downtime.

When several services are deployed together, phases 1-4 run for all of them before anything is routed. If any service fails, the new containers of every service are removed and nothing changes.

### Interrupting a deploy (Ctrl-C)

- Before phase 5: korgi removes every container it created and exits; old containers were never touched.
- From phase 5 on: korgi finishes the syncs and the drain so routing and containers stay consistent, then exits.

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
  - Korgi metadata labels (`korgi.project`, `korgi.service`, `korgi.generation`, `korgi.instance`, `korgi.image`, `korgi.host_port`)
  - Traefik routing labels (if service has `[routing]`)
  - Host port bindings (if `host_base` configured)
  - Docker HEALTHCHECK (if health mode is `docker`)
- If creating or starting any container fails, every container created so far is removed

### 4. HEALTH CHECK

- **Docker / image mode**: Polls `docker inspect` until `.State.Health.Status == "healthy"`
- **HTTP mode**: Makes HTTP requests to `internal_address:host_port/health_path`, using the port the container actually bound
- **No health config**: containers must stay running with a stable restart count for `start_delay`
- All containers are checked concurrently under one deadline (see [Health Checks](Health-Checks))
- On failure: removes ALL new containers, old generation keeps serving

### 5. SYNC CONFIG

- Regenerates Traefik file-provider YAML with the new container topology
- Fails if Traefik is missing or stopped on any LB host (checked before anything is written)
- Writes it into each Traefik container atomically: to `/etc/korgi/.dynamic.yml.tmp`, then `mv` over `/etc/korgi/dynamic.yml`
- Reads the file back and compares it; any mismatch or failed command fails the sync
- **Traffic now routes to old and new containers** -- old containers are only stopped once this succeeded everywhere

### 6. DRAIN OLD

- Stops ALL running containers from other generations (not just N-1), concurrently
- Sends `docker stop -t {drain_seconds}` -- SIGTERM, wait, then SIGKILL
- Refuses to run if the new generation has no running containers
- Safe because Traefik was already updated in phase 5

### 7. SYNC CONFIG

- Regenerates the config without the drained containers
- Always attempted once phase 5 succeeded, even if some stops failed

### 8. CLEANUP

- Keeps the `rollback_keep` stopped generations that served most recently (by container start time, so a known-good generation you rolled back to is kept even if higher-numbered generations exist)
- Never removes the current generation, the rollback target, or any running container

## Port allocation

When using `host_base` ports, each generation gets its own block of `replicas` consecutive ports so old and new containers can run side by side:

- Generation 1 prefers `host_base + 0`, `host_base + 1`, ...
- Generation 2 prefers `host_base + replicas`, `host_base + replicas + 1`, ...

If the preferred block is occupied -- by a running container, or by the recorded port of any stopped korgi container (rollback targets keep their ports) -- korgi moves to the next free block, wrapping around within `host_base..=65535`.

The port each container got is recorded in its `korgi.host_port` label. Traefik routing and HTTP health checks read it from there; it is never recomputed from the config. Containers created by older korgi versions without the label are handled by reading their Docker port bindings.

## Error boundaries

The critical invariant: **the old generation is never stopped until Traefik has been updated to route traffic to the new containers.**

| Phase | Failure | State |
|-------|---------|-------|
| PULL | Image not found | No changes made |
| START GREEN | Container creation fails | Clean up new containers, old gen serves |
| HEALTH CHECK | Timeout or unhealthy | Remove new containers, old gen serves |
| SYNC CONFIG | Traefik missing/stopped, write or read-back fails | Nothing drained; old and new both running |
| DRAIN OLD | Stop fails | New gen serving; final sync still runs; error reported |
| CLEANUP | Remove fails | Stale containers remain (harmless) |

## Rollback

`korgi rollback --service <name>` targets the highest stopped generation below the current one (the current generation is the highest one with running containers) and:

1. Verifies the image exists (re-pulls with registry credentials if garbage-collected)
2. Starts the target's containers (labels and host ports still intact)
3. Health-checks them (same rules as a deploy); on failure they are stopped again and nothing else changes
4. Syncs Traefik (target and current both routed)
5. Stops every other running generation
6. Syncs Traefik again

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
