# Korgi — Future Enhancements

Items not in v1 but worth building later, roughly prioritized.

---

## High Priority

### Advisory deploy locking
Prevent concurrent deploys from racing on generation numbers. Approach: create a short-lived lock container (`korgi-lock-{project}-{service}`) on a designated host. Check for lock before deploying.

### Image digest pinning
Resolve mutable tags (`:latest`) to immutable digests (`@sha256:...`) before deploying. Store the digest in the `korgi.image_digest` label. This ensures all hosts run the exact same image and makes rollback reliable even after re-tags.

```
korgi deploy --resolve-tags  # resolves :latest → @sha256:abc123
```

### Env file support
Load environment variables from a file instead of (or in addition to) the system environment:
```toml
[project]
env_file = ".env.production"
```

### Deploy audit log
Append deploy events to a local log file (`~/.korgi/deploys.log`) with timestamp, service, generation, image, duration, and outcome. Useful for debugging "who deployed what when."

### Image digest verification across hosts
After pulling an image on multiple hosts, compare the digests to ensure they match. Mutable tags like `:latest` can resolve differently if the registry was updated between pulls.

---

## Medium Priority

### Pinned placement strategy
Allow explicit host assignment for stateful services:
```toml
[[services]]
name = "redis"
[services.placement]
strategy = "pinned"
hosts = ["db1"]
```

### Parallel deploy across services
When deploying all services, deploy independent services in parallel rather than sequentially. Services with dependencies would need an explicit ordering mechanism.

### Deploy hooks
Run commands before/after deployment phases:
```toml
[services.deploy]
pre_deploy = "korgi exec --service api -- rails db:migrate"
post_deploy = "curl -X POST https://slack.com/webhook -d 'Deployed!'"
```

### Traefik TCP routing
Support non-HTTP routing for databases, message broues, etc.:
```toml
[services.routing]
rule = "HostSNI(`redis.internal`)"
protocol = "tcp"
entrypoints = ["internal"]
```

### `korgi build` command
Optional image building via SSH exec:
```
korgi build --service api --host build1  # runs docker build on a host
```
Not a core feature but convenient for dev workflows.

### Weighted placement
Hosts with different capacities get proportional container placement:
```toml
[[hosts]]
name = "big-box"
weight = 3  # gets 3x more containers than weight=1 hosts
```

---

## Low Priority

### Secret backend plugins
Support for encrypted secrets (SOPS, Vault, AWS Secrets Manager):
```toml
[secrets]
backend = "sops"
file = "secrets.enc.yaml"
```

### Deployment notifications
Webhook/Slack notifications on deploy success/failure:
```toml
[notifications]
slack_webhook = "${SLACK_WEBHOOK}"
on = ["deploy_success", "deploy_failure", "rollback"]
```

### `korgi events` command
Tail Docker events filtered to korgi-managed containers. Useful for debugging.

### Prometheus metrics
Expose deployment metrics (deploy count, duration, failure rate) via a local endpoint. Conflicts with the "no agents" philosophy but useful for CI/CD dashboards.

### Container log aggregation
Forward container logs to a central logging system (Loki, Elasticsearch):
```toml
[logging]
driver = "json-file"
options = { max-size = "10m", max-file = "3" }
```

### Multi-project support
Manage multiple projects from a single korgi installation. Currently each project has its own `korgi.toml`.

### Config validation command
`korgi validate` that checks config without connecting to any hosts. Currently `korgi check` does both config validation and connectivity testing.

### Dry-run for all commands
Currently only `korgi deploy --dry-run` is supported. Extend to `scale`, `destroy`, `rollback`.

---

## Non-Goals (explicitly out of scope)

### Overlay networking
Cross-host private networking requires Docker Swarm, Kubernetes, or a VPN. Korgi will not manage this. See ADR-007.

### Container orchestration scheduler
Korgi uses explicit placement, not a scheduler. If you need bin-packing, affinity/anti-affinity rules, or preemption — use Kubernetes.

### Image registry management
Korgi pulls images but does not manage registries, garbage collection, or replication.

### Host provisioning
Korgi assumes hosts are already provisioned with Docker installed. Use Terraform/Ansible for host setup.

### Database migrations
Korgi can run commands in containers (`korgi exec`), but does not have a built-in migration system. Use deploy hooks or a separate migration step in CI/CD.
