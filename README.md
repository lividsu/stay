# Stay

A tiny terminal session keeper for Linux.

Run a command, press Ctrl+A to leave, come back with `stay <name>`.

```bash
stay api -- uvicorn main:app --host 0.0.0.0 --port 8000
```

Detach:

```text
Ctrl+A
```

Reattach:

```bash
stay api
```

## Commands

```bash
stay <name>
stay <name> -- <command>
stay ls
stay kill <name>
stay rm <name>
```

Session state is stored under `~/.local/state/stay/`.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/lividsu/stay/main/install.sh | sh
```

The installer downloads the latest Linux static release binary to `~/.local/bin/stay`.
If no release binary is available yet, it falls back to building from source with
the server's local Rust toolchain.

After installing:

```bash
stay --version
```

If `~/.local/bin` is not in `PATH`, the installer prints the line to add to your
shell config.

## Build

```bash
cargo build --release
```

Stay V1 targets Linux only.

## Release

Create a version tag to publish installable Linux binaries:

```bash
git tag v0.1.0
git push origin v0.1.0
```

GitHub Actions builds and uploads:

```text
stay-x86_64-linux-musl
stay-aarch64-linux-musl
```

Once the release workflow completes, this command works on Linux x86_64 and
arm64 servers without requiring Rust:

```bash
curl -fsSL https://raw.githubusercontent.com/lividsu/stay/main/install.sh | sh
```
