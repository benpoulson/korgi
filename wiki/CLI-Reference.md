# CLI Reference

## Global flags

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--config <path>` | `-c` | `korgi.toml` | Path to config file. |
| `--env <name>` | `-e` | | Load `korgi.<name>.toml` overlay. |
| `--json` | | `false` | Output as JSON (where supported). |
| `--yes` | `-y` | `false` | Skip confirmation prompts. |

## Commands

### `korgi init`

Scaffold a new `korgi.toml` configuration file.

```sh
korgi init
```

Fails if `korgi.toml` already exists.

### `korgi check`

Validate configuration, test SSH connectivity, and verify Docker access on all hosts.

```sh
korgi check
```

### `korgi status`

Show running containers across all hosts.

```sh
korgi status
korgi status --service api
korgi status --json
```

| Flag | Description |
|------|-------------|
| `--service <name>` | Filter by service name. |

### `korgi deploy`

Zero-downtime deployment. See [Deployment Pipeline](Deployment-Pipeline) for the full flow.

```sh
korgi deploy                              # deploy all services
korgi deploy --service api                # deploy one service
korgi deploy --service api --image v2.1   # override image
korgi deploy --dry-run                    # preview only
korgi -y deploy                           # skip confirmation
```

| Flag | Description |
|------|-------------|
| `--service <name>` | Deploy only this service. |
| `--image <ref>` | Override the image (useful in CI). |
| `--dry-run` | Show what would happen without making changes. |

### `korgi rollback`

Roll back a service to its previous generation.

```sh
korgi rollback --service api
```

Restarts the most recent stopped generation and stops the current one.

### `korgi scale`

Scale a service to N replicas.

```sh
korgi scale --service api 5
korgi scale --service api 0   # removes all containers
```

### `korgi traefik deploy`

Deploy Traefik to all `role = "lb"` hosts.

```sh
korgi traefik deploy
```

### `korgi traefik status`

Show Traefik status on all LB hosts.

```sh
korgi traefik status
```

### `korgi traefik logs`

Tail Traefik logs.

```sh
korgi traefik logs
korgi traefik logs --follow
```

### `korgi exec`

Run a command in a running container.

```sh
korgi exec --service api -- sh -c "echo hello"
```

Runs in the first running container of the service.

### `korgi logs`

Tail logs from a service.

```sh
korgi logs --service api
korgi logs --service api --follow
```

### `korgi destroy`

Stop and remove containers.

```sh
korgi destroy                    # all services
korgi destroy --service api      # one service
```
