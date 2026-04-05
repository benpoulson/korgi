# Secrets Management

Korgi supports `${VAR}` variable interpolation in config files, resolved from a secrets file and/or system environment variables.

## Secrets file

Point to a KEY=VALUE file in your project config:

```toml
[project]
name = "myapp"
secrets = ".korgi-secrets"
```

The file uses simple KEY=VALUE format:

```
DB_PASSWORD=hunter2
JWT_SECRET=supersecret
GH_TOKEN=ghp_abc123
S3_SECRET_KEY=wJalrXUtnFEMI/K7MDENG
```

- Blank lines and lines starting with `#` are ignored
- Path is relative to the config file's directory
- **The file is optional** -- if it doesn't exist, variables resolve from system env only

## Variable interpolation

Use `${VAR}` in your config to reference secrets:

```toml
[services.env]
DATABASE_URL = "postgres://user:${DB_PASSWORD}@10.0.0.1/mydb"
JWT_SECRET = "${JWT_SECRET}"

[[registries]]
github_token = "${GH_TOKEN}"
```

## Resolution order

1. Secrets file is loaded first
2. System environment variables are merged on top (take precedence)
3. `${VAR}` references are resolved from the combined map

This means you can override secrets file values with environment variables (useful in CI):

```sh
export DB_PASSWORD=ci-test-password
korgi deploy  # uses env var, not secrets file value
```

## Strict mode

Unresolved `${VAR}` references cause a hard error. Korgi never deploys with empty credentials.

```
Error: environment variable 'DB_PASSWORD' is not set
```

## Comments

`${VAR}` in TOML comments (`# lines`) is **not** interpolated, so commented-out examples don't trigger errors:

```toml
# DATABASE_URL = "${DATABASE_URL}"  # this is fine, not evaluated
```

## Security

- Add your secrets file to `.gitignore`
- Never commit secrets to the repository
- Use environment variables in CI/CD pipelines instead of a secrets file
