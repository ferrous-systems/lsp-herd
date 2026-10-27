# lsp-herd

A fork of [lspmux](https://codeberg.org/p2502/lspmux).

Multiplexer for [LSP], allows multiple LSP clients (editor windows) to share a
single language server instance per workspace.

[LSP]: https://microsoft.github.io/language-server-protocol


## How it works

`lsp-herd` acts as a proxy between a language server and a LSP client. it is
composed of two parts.

1. server which listens on a socket, takes care of the actual language server
   lifecycle and multiplexes connections from multiple clients onto the
   appropriate language servers. and
2. client shim which acts like a language server binary but only connects to the
   `lsp-herd` server and forwards all input and output from stdio to the socket.

Some clients, like `nvim`, are able to connect directly to a socket and setup
custom LSP initOptions, for those clients the shim is optional as they can be
configured to connect directly to the `lsp-herd` server.

When the client shim is invoked by an editor, it finds which workspace and
environment it has been spawned in. It then connects to the server and passes
its environment along during the handshake, the server then either connects it
to an existing language server if one with matching workspace and environment is
running or spawn a new one.

Because neither LSP nor language servers (usually) support multiple clients per
server `lsp-herd` intercepts the handshake process and modifies IDs of requests
and responses to track which response belongs to which client. Because not all
messages can be tracked this way it drops some, notably it drops any requests
from the server, this may cause some issues for you, if you run into any issues
which are definitely not present in the language server alone please open an
issue!


## How to use

Install `lsp-herd` using cargo

```sh
$ cargo install --git https://github.com/ferrous-systems/lsp-herd --locked
```

Run `lsp-herd` in server mode, make sure that `lsp-herd` is in your
`PATH`:

```sh
$ which rust-analyzer
/home/user/.cargo/bin/rust-analyzer
$ target/release/lsp-herd --help
share one language server instance between multiple LSP clients to save resources

Usage: lsp-herd [COMMAND]

Commands:
  client  Connect to an lsp-herd server [default]
  server  Start an lsp-herd server
  status  Print server status
  config  Print server configuration
  reload  Reload workspace
  sync    Reload server file state from disk
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

On linux distros with systemd `lsp-herd server` can run as a systemd user
service, see the example `lsp-herd.service`.
On MacOS it can run as a Launchd service, see the example `lsp-herd.plist`.

Configure your editor to use `lsp-herd` as `rust-analyzer`, for example for
CoC in neovim edit `~/.config/nvim/coc-settings.json`, add:

```json
{
    "rust-analyzer.serverPath": "/path/to/lsp-herd"
}
```

If your editor can connect to a language server via TCP you don't need to use
the `lsp-herd` client and connect directly to the server but you need to
provide the same information as the proxy command would. See the
[example config for neovim](examples/neovim/init.lua) for details.


## Configuration

Configuration is stored in a TOML file in your system's default configuration
directory, for example `~/.config/lsp-herd/config.toml`. If you're not sure where
that is on your system starting `lsp-herd` without a config file present will
print a notice with the expected path.

Note that the configuration file is likely not necessary and `lsp-herd` should be
usable with all defaults.

Example configuration file:

```toml
# this is an example configuration file for lsp-herd
#
# all configuration options here are set to their default value they'll have if
# they're not present in the file or if the config file is missing completely.

# time in seconds after which a rust-analyzer server instance with no clients
# connected will get killed to save system memory.
#
# you can set this option to `false` for infinite timeout
instance_timeout = 300 # after 5 minutes

# time in seconds how long to wait between the gc task checks for disconnected
# clients and possibly starts a timeout task. the value must be at least 1.
gc_interval = 10 # every 10 seconds

# when a client opens a workspace inside a git repository, remember that
# workspace shape and preemptively start matching language servers for other
# worktrees of the same repository.
git_worktree_prewarm = true

# time in seconds how long to wait between checks for new or removed git
# worktrees. the value must be at least 1.
git_worktree_poll_interval = 30 # every 30 seconds

# each worktree poll emits an info-level summary of observed repositories,
# worktrees, desired workspace roots, and running language server processes.
# it also emits one info-level line for each tracked worktree workspace with
# its process status, pid, client count, idle time, and worktree change time.
# missing prewarmed instances are started from most recently changed worktree to
# least recently changed, with at most one speculative spawn per minute unless
# the previous server reports that startup/background work is done.

# root directory under which lsp-herd creates rust-analyzer target directories.
# the default ${cache_dir} placeholder expands to your platform cache directory,
# for example ~/.cache/lsp-herd on Linux.
rust_analyzer_target_dir_root = "${cache_dir}/rust-analyzer-targets"

# how rust-analyzer target directories are grouped:
# - "shared" uses one target directory for all rust-analyzer instances.
# - "per-git-remote" uses one target directory per Git origin URL, falling back
#   to the workspace path when no origin is configured.
# - "per-instance" uses one target directory per lsp-herd rust-analyzer instance.
rust_analyzer_target_dir_grouping = "per-instance"

# when lsp-herd server runs with stdout attached to a terminal, show a
# periodically refreshed status UI. non-terminal service runs stay quiet.
status_ui = true

# ip address and port on which lsp-herd server listens
# or unix socket path on *nix operating systems
#
# the default "127.0.0.1" only allows connections from localhost which is
# preferred since the protocol doesn't worry about security.
# lsp-herd server expects the filesystem structure and contents to be the same
# on its machine as on lsp-herd's machine. if you want to run the server on a
# different computer it's theoretically possible but at least for now you're on
# your own.
#
# ports below 1024 will typically require root privileges and should be
# avoided, the default was picked at random, this only needs to change if
# another application happens to collide with lsp-herd.
listen = ["127.0.0.1", 27631] # localhost & some random unprivileged port
# listen = "/var/run/lsp-herd/lsp-herd.sock" # unix socket

# ip address and port to which lsp-herd will connect to or unix socket path on
# *nix operating systems
#
# this should usually just match the value of `listen`
connect = ["127.0.0.1", 27631] # same as `listen`
# connect = "/var/run/lsp-herd/lsp-herd.sock" # same as `listen`

# default log filters
#
# RUST_LOG env variable overrides this option, both use the same syntax which
# is documented in the `env_logger` documentation here:
# <https://docs.rs/env_logger/0.9.0/env_logger/index.html#enabling-logging>
log_filters = "info"

# filter environment variables passed from `lsp-herd client` to the server
#
# The filters are separated into positive and negative (prepended by `!`). For
# an environment variable to be passed its name must match at least one positive
# filter (the default filter set is ["*"] which passes everything), and must not
# match any negative filters. The full glob syntax is described in the `glob`
# crate documentation <https://docs.rs/glob/latest/glob/struct.Pattern.html>,
# except the leading `!` for negative patterns which is parsed and removed by
# `lsp-herd`.
#
# If the "PATH" variable is passed to the server then it'll be also used for
# looking up a relative argument to `--server-path`.
#
# One way to filter variables is to explicitly list every variable you want to
# pass to the language server.
# (This is backward compatible with behaviour prior to v0.3.1)
#
# Example: pass_environment = ["PATH", "LD_LIBRARY_PATH"]
#
# Another option is to allow everything and then use negative filters to exclude
# variables which cause problems - variables like WINDOWID set by the terminal
# emulator which would cause `lsp-herd` to spawn a new language server for every
# connected client, defeating its purpose.
#
# Example: pass_environment = ["*", "!DESKTOP_STARTUP_ID", "!WINDOWID", "!ALACRITTY_*"]
pass_environment = ["*"]
```


## Other LSP servers

By default `lsp-herd` uses a `rust-analyzer` binary found in its `$PATH` as the
server. This can be overridden using the `--server-path` cli option with the
client subcommand or `LSP_HERD_SERVER` environment variable. You can usually
configure one of these in your editor configuration. If both are specified the
cli option overrides the environment variable.

For example with `coc-clangd` in CoC for neovim add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/home/user/.cargo/bin/lsp-herd",
    "clangd.arguments": ["client", "--server-path", "/usr/bin/clangd"]
}
```

Or to set a custom path for `rust-analyzer` with `coc-rust-analyzer` add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "rust-analyzer.server.path": "/home/user/.cargo/bin/lsp-herd",
    "rust-analyzer.server.extraEnv": { "LSP_HERD_SERVER": "/custom/path/rust-analyzer" }
}
```

If your editor configuration or plugin doesn't allow to add either you can
instead create a wrapper shell script and set it as the server path directly.
For example if `coc-clangd` didn't allow to pass additional arguments you'd
need a script like `/usr/local/bin/clangd-proxy`:

```sh
#!/bin/sh
exec /home/user/.cargo/bin/lsp-herd client --server-path /usr/bin/clangd $@
```

And configure the editor to use the wrapper script in
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/usr/local/bin/clangd-proxy"
}
```
