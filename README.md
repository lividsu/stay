# Stay

A tiny terminal session keeper for Linux.

Run a command, press Ctrl+A to leave, come back with `stay <name>`.
Stay opens the session in a clean alternate screen, then restores your previous
terminal history when you return.

## Why Stay

Stay is for people who only need the one good part of `screen`: keep a terminal
task running after SSH disconnects.

Compared with `screen`, Stay keeps the workflow smaller:

- Easy commands: use `stay api`, `stay ls`, `stay kill api`, and `stay rm api`.
- Portal-like attach: `stay api` clears into its own screen, and Ctrl+A brings
  your old terminal view back.
- Smart tab completion: shell completions can suggest existing session names.
- Native copy and scroll: Stay does not add its own copy mode, so you keep using
  your terminal's normal scrollback and copy behavior.
- Persistent records: after a reboot, running tasks are gone, but Stay keeps the
  session name, working directory, and last command so you can restart cleanly.
- Tiny footprint: Stay is a small local daemon with one PTY per active session,
  and it does almost nothing while your task runs.

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
stay completions <bash|zsh|fish>
```

Session state is stored under `~/.local/state/stay/`.

## Install

```bash
curl -fsSL https://raw.githubusercontent.com/lividsu/stay/main/install.sh | sh
```

The installer downloads the latest Linux static release binary to `~/.local/bin/stay`.
If no release binary is available yet, it falls back to building from source with
the server's local Rust toolchain. It also installs shell completions for bash,
zsh, and fish where those shells look for user completions.

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

## Shell Completion

Type `stay <Tab>` to complete commands and existing session names.

The installer sets this up for you: it writes the completion files and adds a
line to `~/.bashrc` / `~/.zshrc` so completion loads in new shells. Open a new
terminal after installing. (Set `STAY_NO_RC=1` to skip the shell config edits.)

If you built from source with `cargo build`, enable it yourself.

bash — add to `~/.bashrc`:

```bash
eval "$(stay completions bash)"
```

fish — add to `~/.config/fish/config.fish`:

```fish
stay completions fish | source
```

zsh loads completions as autoloaded functions on `fpath`, so write the file and
initialize the completion system in `~/.zshrc`:

```zsh
stay completions zsh > ~/.zsh/completions/_stay
fpath=(~/.zsh/completions $fpath)
autoload -Uz compinit && compinit
```

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
