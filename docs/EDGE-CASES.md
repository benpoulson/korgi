# Korgi -- Edge Cases & Failure Modes

## Deployment Failures

### Image pull fails on one host but succeeds on others

**Behavior:** Deploy aborts entirely. No containers are started.  
**State after failure:** Images are pulled on some hosts but no containers created.  
**Recovery:** Re-run `korgi deploy`. Hosts that already have the image will skip the pull.  
**Improvement opportunity:** Pull in parallel across hosts, report which succeeded/failed.

### Green container fails to start (e.g., port conflict)

**Behavior:** Any green container start failure triggers cleanup of ALL green containers for this generation.  
**State after failure:** Old generation continues serving.  
**Recovery:** Fix the root cause (port conflict, missing volume, etc.) and re-deploy.

### Health check times out

**Behavior:** All green containers are stopped and removed. Old generation continues serving.  
**State after failure:** Clean -- only old generation running.  
**Recovery:** Check container logs (`korgi logs --service X`), fix health check or application, re-deploy.  
**Note:** The deadline is `start_period + (interval + timeout) * (retries + 1) + start_delay + 15s`; all new containers are checked concurrently under it.

### Traefik missing, stopped or not updated on one LB host

**Behavior:** The sync fails before anything is written, or (for a write/read-back failure) as soon as that host fails. Old containers are **not** drained.  
**State after failure:** Old and new generations both running. Hosts already updated route to both, which is safe.  
**Recovery:** `korgi traefik deploy` (or fix the host), then `korgi deploy` again.

### One of several services fails

**Behavior:** Start and health check run for every selected service before any routing change. If one fails, the new containers of every service are removed.  
**State after failure:** Exactly as before the deploy.

### Ctrl-C during a deploy

**Before the routing switch:** every container created so far is removed; old containers untouched.  
**After the routing switch:** korgi finishes the drain and the final Traefik sync before exiting.  
**Caveat:** an interrupt landing exactly while Docker is creating a container can leave that one container behind (Docker created it but the call never returned); `korgi status` shows it and the next deploy's cleanup or `korgi destroy` removes it.

### SSH connection drops mid-deploy

**Behavior:** Depends on which phase:
- During PULL: partial pulls on some hosts. Re-running will resume.
- During START GREEN / HEALTH CHECK: korgi tries to remove the green containers; if the host is unreachable they remain (not routed). Old generation still running.
- During DRAIN OLD: some old containers stopped, some not. Green is serving.  

**Recovery:** Run `korgi status` to see the mess. Then either:
- `korgi deploy` again (will create a new generation, clean up the partial one)
- Manually clean up with `korgi destroy --service X` and re-deploy

### Docker daemon unresponsive on one host

**Behavior:** `DockerHost::connect()` fails with timeout.  
**State after failure:** No changes made.  
**Recovery:** Fix Docker on the host, then retry.

## Scaling Failures

### Scale down while deploy is in progress

**Behavior:** Race condition -- scale may remove containers from the in-progress generation.  
**Mitigation:** Not currently prevented. Serialize operations manually.  
**Future:** Advisory locking (ADR-006).

### Scale to 0

**Behavior:** All running containers for the service are stopped and removed.  
**Note:** This is equivalent to `korgi destroy --service X` for the current generation.

## Rollback Failures

### Image garbage-collected

**Behavior:** Korgi detects the missing image via `docker image inspect`, re-pulls it, then starts the old containers.  
**Prerequisite:** The `korgi.image` label on stopped containers must contain a valid, pullable image reference.  
**Failure mode:** If the image no longer exists in the registry, rollback fails.

### Rollback target does not become healthy

**Behavior:** The target's containers are stopped again; the current generation is never touched or un-routed.

### No previous generation to roll back to

**Behavior:** Error message: "No previous generation found to roll back to."  
**Cause:** Either no previous deploys, or old generations were cleaned up (exceeded `rollback_keep`).

### Old containers were removed manually

**Behavior:** Korgi can't find stopped containers for the rollback generation.  
**Recovery:** Re-deploy the old image: `korgi deploy --service X --image myapp/api:v1`

## Traefik Issues

### Traefik can't reach container

**Cause:** Container is not on the same Docker network as Traefik.  
**Symptom:** Traefik shows the backend but returns 502/504.  
**Fix:** Ensure `[traefik].network` matches the network containers are attached to. Korgi handles this automatically.

### ACME certificate rate limits

**Cause:** Let's Encrypt has rate limits (50 certs/week for the same domain).  
**Symptom:** Traefik logs show ACME errors, HTTPS doesn't work.  
**Fix:** Use staging ACME for testing. Check Traefik logs: `korgi traefik logs`

### Traefik container restarts in a loop

**Cause:** Usually a port conflict (port 80/443 already in use).  
**Diagnosis:** `korgi traefik logs` or `docker logs korgi-traefik` on the host.

## Config Issues

### Unset environment variable

**Behavior:** Hard error during config loading. Korgi refuses to proceed with unset `${VAR}` references.  
**Rationale:** Deploying with empty credentials is worse than failing fast.

### Overlay file not found

**Behavior:** Error if `--env staging` is specified but `korgi.staging.toml` doesn't exist.

### Placement labels match no hosts

**Behavior:** Validation error at config load time.  
**Message:** "service 'api' placement_labels ["gpu"] don't match any host"

### Duplicate service names

**Behavior:** TOML parsing succeeds (arrays allow duplicates), but korgi treats them as separate array entries. The last one wins for operations that filter by name.  
**Improvement opportunity:** Add validation to reject duplicate service names.

## Concurrency Issues

### Two users deploy the same service simultaneously

**Behavior:** Both see the same current generation N, both try to create generation N+1.  
**What happens:** The second deployer's containers will have the same generation number. Depending on timing:
- If second runs during first's PULL phase: duplicate container names cause creation failures
- If second runs after first's DRAIN phase: second deploy sees first's containers as current  

**Mitigation:** CI/CD pipeline serialization. Future: advisory locks (ADR-006).

### Deploy during scale operation

**Behavior:** Scale modifies the current generation. Deploy creates a new generation based on stale state.  
**Result:** Deploy's placement may be incorrect (e.g., scaling added containers that deploy doesn't know about).  
**Mitigation:** Serialize operations. The state re-query in deploy's PREPARE phase partially mitigates this.

## Resource Issues

### Container OOM killed

**Behavior:** Docker kills the container. If it has a restart policy (`unless-stopped`), it restarts.  
**Symptom:** Container shows as "running" but keeps restarting. Health checks may flip between healthy/unhealthy.  
**Fix:** Increase `[services.resources].memory` and re-deploy.

### Disk full on host

**Behavior:** Image pulls fail, container creation fails, log writes fail.  
**Diagnosis:** SSH into host, check `df -h` and `docker system df`.  
**Fix:** `docker system prune` on the host, then retry.

### Docker socket permission denied

**Behavior:** `DockerHost::connect()` fails with permission error.  
**Fix:** Ensure the SSH user is in the `docker` group on the remote host.
