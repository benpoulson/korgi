# Troubleshooting

## SSH connection issues

### "Permission denied (publickey,password)"

Your SSH key isn't authorized on the server, or the key format isn't supported.

**Check**: `ssh -i ~/.ssh/id_rsa user@host "echo ok"`

**Fixes**:
- Add your public key to the server's `~/.ssh/authorized_keys`
- Ensure the SSH user is correct in `korgi.toml`
- If using a passphrase, korgi will prompt for it

### "Connection refused" or "Connection timed out"

The host is unreachable or SSH isn't running.

**Check**: `ssh user@host` or `nc -z host 22`

**Fixes**:
- Verify the `address` in korgi.toml
- Check firewall rules
- If using a non-standard port, set `port = 2222` in the host config

### Passphrase prompt appears twice

If you have multiple hosts, korgi authenticates to each one separately. Each connection may prompt for the passphrase.

**Fix**: Use ssh-agent to load the key once: `ssh-add ~/.ssh/id_rsa`

## Docker issues

### "Docker ping failed"

SSH connects but Docker is unreachable.

**Fixes**:
- Ensure Docker is installed and running on the host
- Ensure the SSH user is in the `docker` group: `sudo usermod -aG docker deploy`
- Check the Docker socket exists: `ls -la /var/run/docker.sock`

### "Failed to open channel to /var/run/docker.sock"

The SSH tunnel to the Docker socket failed.

**Fixes**:
- Check Docker socket permissions on the host
- If using a custom socket path, set `docker_socket` in the host config

## Deployment issues

### "Health check failed: /bin/sh: no such file or directory"

The container image has no shell (e.g. `FROM scratch`). Docker HEALTHCHECK can't run.

**Fix**: Use HTTP health check mode:

```toml
[services.health]
mode = "http"
path = "/health"
```

### "Health check timed out"

The container started but didn't become healthy in time.

**Fixes**:
- Check container logs: `korgi logs --service <name>`
- Increase `drain_seconds` (health timeout = `drain_seconds * 2`)
- Add `start_period` to allow initialization time
- Verify the health endpoint works: `curl http://host:port/health`

### "No hosts match placement labels"

No host has the labels required by the service.

**Fix**: Check `placement_labels` on the service matches `labels` on at least one host.

### Image pull fails

**Fixes**:
- Verify the image exists in the registry
- Check registry credentials in `[[registries]]`
- For GHCR, ensure your token has `read:packages` scope

## Config issues

### "Environment variable 'X' is not set"

A `${VAR}` reference couldn't be resolved from the secrets file or system environment.

**Fixes**:
- Check your secrets file exists at the path specified in `project.secrets`
- Verify the variable is defined in the secrets file (KEY=VALUE format)
- Or set it as an environment variable: `export VAR=value`

### "Duplicate service name"

Two `[[services]]` entries have the same `name`.

**Fix**: Each service must have a unique name.

### "[traefik] is configured but no hosts have role = "lb""

The `[traefik]` section exists but no host has `role = "lb"` or `role = "both"`.

**Fix**: Add `role = "lb"` or `role = "both"` to at least one host.

## Debug logging

For detailed output, set `RUST_LOG`:

```sh
RUST_LOG=debug korgi status
RUST_LOG=debug korgi deploy
```
