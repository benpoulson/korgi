# Installation

## One-liner

```sh
curl -sL https://raw.githubusercontent.com/benpoulson/korgi/master/install.sh | sh
```

Detects your OS and architecture automatically. Supports macOS (Intel/Apple Silicon) and Linux (x86_64/ARM64).

## From source

```sh
git clone https://github.com/benpoulson/korgi.git
cd korgi
cargo install --path .
```

## Manual download

Download the binary for your platform from [Releases](https://github.com/benpoulson/korgi/releases):

| Platform | Binary |
|----------|--------|
| macOS Apple Silicon | `korgi-macos-arm64.tar.gz` |
| macOS Intel | `korgi-macos-amd64.tar.gz` |
| Linux x86_64 | `korgi-linux-amd64.tar.gz` |
| Linux ARM64 | `korgi-linux-arm64.tar.gz` |

```sh
tar xzf korgi-*.tar.gz
sudo mv korgi /usr/local/bin/
```

## Prerequisites

- SSH access to your target hosts (key-based auth, passphrases supported)
- Docker installed on target hosts
- SSH user in the `docker` group
