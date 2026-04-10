# SSH Authentication

Korgi uses pure Rust SSH (via the `ssh2` crate / libssh2) for all host connections. No system `ssh` binary is needed.

## Authentication flow

Korgi tries authentication methods in this order:

1. **Host key verification** -- verify the remote host against `~/.ssh/known_hosts`
2. **Key files** -- explicit `ssh_key` from config, or default paths
3. **ssh-agent** -- if no key file works
4. **Password** -- interactive prompt as last resort

## Host key verification

Korgi verifies every SSH host against your standard OpenSSH `~/.ssh/known_hosts` file.

- **Match** -- connection continues
- **Missing host key** -- in an interactive terminal, Korgi prompts to trust the key and writes it to `known_hosts`
- **Missing host key in non-interactive mode** -- connection fails and you must pre-populate `known_hosts`
- **Mismatch** -- connection fails hard to avoid connecting to the wrong host

## Key files

### Explicit key

```toml
[[hosts]]
name = "server"
address = "10.0.0.1"
user = "deploy"
ssh_key = "~/.ssh/deploy_key"
```

### Default key paths

If no `ssh_key` is set, korgi tries these in order:

1. `~/.ssh/id_ed25519`
2. `~/.ssh/id_rsa`
3. `~/.ssh/id_ecdsa`

### Passphrase-protected keys

If a key is encrypted, korgi prompts for the passphrase:

```
Enter passphrase for /Users/you/.ssh/id_rsa (attempt 1/3):
```

Input is hidden (no characters shown). If the passphrase is wrong, korgi prompts again up to 3 times before moving on to other authentication methods.

## ssh-agent

If key file authentication fails, korgi tries the SSH agent. This works automatically if your agent has keys loaded:

```sh
ssh-add ~/.ssh/id_rsa
```

## Password authentication

If both key and agent auth fail, korgi prompts for a password:

```
deploy@10.0.0.1's password:
```

## `korgi check`

`korgi check` is the main diagnostics command. It reports:

- config sanity
- SSH reachability
- host key verification
- authentication
- Docker reachability over the SSH tunnel

Use `korgi check --json` for machine-readable output.

## Supported key types

- Ed25519
- RSA (all sizes, SHA-256/SHA-512 signatures)
- ECDSA (P-256, P-384, P-521)

## SSH port

Non-standard SSH ports are supported:

```toml
[[hosts]]
name = "server"
address = "10.0.0.1"
port = 2222
```

## Docker socket tunneling

Korgi tunnels Docker API calls through SSH using `channel_direct_streamlocal`. This opens a direct connection to `/var/run/docker.sock` on the remote host without exposing Docker over TCP.

The Docker socket path can be customized:

```toml
[[hosts]]
docker_socket = "/run/docker.sock"
```
