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
**Note:** The timeout is `drain_seconds * 2` by default.

### SSH connection drops mid-deploy

**Detection:** Docker calls on that host fail instead of hanging: every API request has a 120s client timeout
(stop requests: the container's stop grace period plus 30s), image pulls fail after 120s without progress, and
commands run inside containers fail after 60s. SSH keepalives are sent every 15s so idle connections are not
dropped by NAT or firewalls; they do not by themselves detect a peer that vanished without closing the TCP
connection -- the request timeouts above do.

**Behavior:** Depends on which phase:
- During PULL: partial pulls on some hosts. Re-running will resume.
- During START GREEN: some green containers may be running without health check verification. Old generation still running.
- During DRAIN OLD: some old containers stopped, some not. Green is serving.  

**Recovery:** Run `korgi status` to see the mess. Then either:
- `korgi deploy` again (will create a new generation, clean up the partial one)
- Manually clean up with `korgi destroy --service X` and re-deploy

### Docker daemon unresponsive on one host

**Behavior:** `DockerHost::connect()` fails when the Docker ping times out (120s). Later calls on an
unresponsive daemon fail with the timeouts listed under "SSH connection drops mid-deploy".  
**State after failure:** No changes made.  
**Recovery:** Fix Docker on the host, then retry.

### Image pull stalls

**Behavior:** The pull fails with "no progress for 120s" when the registry or daemon stops reporting
progress, or "did not finish within 1800s" for a pull that keeps trickling. The deploy aborts as for any
pull failure.

## SSH Transport

### Host unreachable

**Behavior:** The TCP connect gives up after 10s per resolved address (override with
`KORGI_SSH_CONNECT_TIMEOUT=<seconds>`). Each blocking step of the SSH setup (handshake, host key, authentication
exchanges) gives up after 30s. DNS resolution uses the system resolver and its own timeouts.

### Encrypted key or password auth without a terminal (CI, cron)

**Behavior:** Korgi never waits for a passphrase or password it cannot read. Without an interactive terminal an
encrypted key is skipped (ssh-agent is still tried), and if nothing else authenticates the connection fails
immediately with an error naming the key. Load the key into ssh-agent or use an unencrypted deploy key.

### How the Docker tunnel behaves

bollard connects to a local Unix socket in a private per-process directory (`$TMPDIR/korgi-<pid>-...`, mode
0700, removed when the connection is dropped). Every connection bollard opens gets its own SSH
`direct-streamlocal` channel to the remote Docker socket, and one thread per host pumps all of them without
blocking, so a kept-alive idle connection, a streaming response (`logs --follow`) or a large request never stalls
another request to the same host. A connection ends when either side closes it; if the local side finishes
sending and Docker never closes, it is dropped after 120s. A failed channel or socket closes the local connection,
which surfaces as an error from the Docker call rather than a hang.

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

### Command inside a container fails

**Behavior:** Writing the Traefik config (and any other `docker exec` korgi runs) fails with the command's exit
code and stderr if the command exits non-zero, the output stream breaks, or it runs longer than 60s.
`korgi exec` reports a non-zero exit code of the command as an error.

### Docker socket permission denied

**Behavior:** `DockerHost::connect()` fails with permission error.  
**Fix:** Ensure the SSH user is in the `docker` group on the remote host.
