# Getting Started

## 1. Initialize

```sh
korgi init
```

Creates a `korgi.toml` template with example configuration.

## 2. Configure

Edit `korgi.toml` with your hosts, services, and Traefik settings:

```toml
[project]
name = "myapp"
secrets = ".korgi-secrets"

# Load balancer
[[hosts]]
name = "lb"
role = "lb"
address = "203.0.113.1"
internal_address = "10.0.0.1"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"

# Worker node
[[hosts]]
name = "worker-1"
address = "10.0.0.10"
internal_address = "10.0.0.10"
user = "deploy"
ssh_key = "~/.ssh/id_ed25519"
labels = ["app"]

[traefik]
image = "traefik:v3.2"
entrypoints = { web = ":80", websecure = ":443" }
network = "korgi-traefik"

[traefik.acme]
email = "admin@example.com"
storage = "/letsencrypt/acme.json"

[[registries]]
github_token = "${GH_TOKEN}"

[[services]]
name = "api"
image = "ghcr.io/myorg/api:latest"
replicas = 2
placement_labels = ["app"]

[services.health]
mode = "http"
path = "/health"

[services.routing]
rule = "Host(`api.example.com`)"
entrypoints = ["websecure"]
tls = true

[services.ports]
container = 8080
host_base = 9001

[services.deploy]
drain_seconds = 30
rollback_keep = 2
```

## 3. Validate

```sh
korgi check
```

Tests SSH connectivity and Docker access on all hosts.

## 4. Deploy Traefik

```sh
korgi traefik deploy
```

## 5. Deploy services

```sh
korgi deploy
```

## 6. Monitor

```sh
korgi status
```
