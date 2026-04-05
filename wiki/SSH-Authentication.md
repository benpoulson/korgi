# SSH Authentication

Korgi uses pure Rust SSH (via the `ssh2` crate / libssh2) for all host connections. No system `ssh` binary is needed.

## Authentication flow

Korgi tries authentication methods in this order:

1. **Key files** -- explicit `ssh_key` from config, or default paths
2. **ssh-agent** -- if no key file works
3. **Password** -- interactive prompt as last resort

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
Enter passphrase for /Users/you/.ssh/id_rsa:
```

Input is hidden (no characters shown).

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
