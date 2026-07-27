# lspmux &emsp; [![Latest Version]][crates.io]

[Latest Version]: https://img.shields.io/crates/v/lspmux.svg
[crates.io]: https://crates.io/crates/lspmux

Multiplexer for [LSP], allows multiple LSP clients (editor windows) to share a
single language server instance per workspace.

[LSP]: https://microsoft.github.io/language-server-protocol


## How it works

`lspmux` acts as a proxy between a language server and a LSP client. it is
composed of two parts.

1. server which listens on a socket, takes care of the actual language server
   lifecycle and multiplexes connections from multiple clients onto the
   appropriate language servers. and
2. client shim which acts like a language server binary but only connects to the
   `lspmux` server and forwards all input and output from stdio to the socket.

Some clients, like `nvim`, are able to connect directly to a socket and setup
custom LSP initOptions, for those clients the shim is optional as they can be
configured to connect directly to the `lspmux` server.

When the client shim is invoked by an editor, it finds which workspace and
environment it has been spawned in. It then connects to the server and passes
its environment along during the handshake, the server then either connects it
to an existing language server if one with matching workspace and environment is
running or spawn a new one.

Because neither LSP nor language servers (usually) support multiple clients per
server `lspmux` intercepts the handshake process and modifies IDs of requests
and responses to track which response belongs to which client. Because not all
messages can be tracked this way it drops some, notably it drops any requests
from the server, this may cause some issues for you, if you run into any issues
which are definitely not present in the language server alone please open an
issue!


## How to use

Install `lspmux` using cargo

```sh
$ cargo install lspmux
```

or using one of the package managers which has it
[available in its repository](https://repology.org/project/lspmux/versions).

Run `lspmux` in server mode, make sure that `lspmux` is in your
`PATH`:

```sh
$ which rust-analyzer
/home/user/.cargo/bin/rust-analyzer
$ target/release/lspmux --help
share one language server instance between multiple LSP clients to save resources

Usage: lspmux [COMMAND]

Commands:
  client  Connect to an lspmux server [default]
  server  Start a lpsmux server
  status  Print server status
  config  Print server configuration
  reload  Reload workspace
  sync    Reload server file state from disk
  help    Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

On linux distros with systemd `lspmux server` can run as a systemd user
service, see the example `lspmux.service`.
On MacOS it can run as a Launchd service, see the example `lspmux.plist`.

Configure your editor to use `lspmux` as `rust-analyzer`, for example for
CoC in neovim edit `~/.config/nvim/coc-settings.json`, add:

```json
{
    "rust-analyzer.serverPath": "/path/to/lspmux"
}
```

If your editor can connect to a language server via TCP you don't need to use
the `lspmux` client and connect directly to the server but you need to
provide the same information as the proxy command would. See the
[example config for neovim](examples/neovim/init.lua) for details.


## Configuration

Configuration is stored in a TOML file in your system's default configuration
directory, for example `~/.config/lspmux/config.toml`. If you're not sure where
that is on your system starting `lspmux` without a config file present will
print a notice with the expected path.

Note that the configuration file is likely not necessary and `lspmux` should be
usable with all defaults.

Example configuration file:

```toml
# this is an example configuration file for lspmux
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
# its process status, pid, client count, and idle time.

# ip address and port on which lspmux server listens
# or unix socket path on *nix operating systems
#
# the default "127.0.0.1" only allows connections from localhost which is
# preferred since the protocol doesn't worry about security.
# lspmux server expects the filesystem structure and contents to be the same
# on its machine as on lspmux's machine. if you want to run the server on a
# different computer it's theoretically possible but at least for now you're on
# your own.
#
# ports below 1024 will typically require root privileges and should be
# avoided, the default was picked at random, this only needs to change if
# another application happens to collide with lspmux.
listen = ["127.0.0.1", 27631] # localhost & some random unprivileged port
# listen = "/var/run/lspmux/lspmux.sock" # unix socket

# ip address and port to which lspmux will connect to or unix socket path on
# *nix operating systems
#
# this should usually just match the value of `listen`
connect = ["127.0.0.1", 27631] # same as `listen`
# connect = "/var/run/lspmux/lspmux.sock" # same as `listen`

# default log filters
#
# RUST_LOG env variable overrides this option, both use the same syntax which
# is documented in the `env_logger` documentation here:
# <https://docs.rs/env_logger/0.9.0/env_logger/index.html#enabling-logging>
log_filters = "info"

# filter environment variables passed from `lspmux client` to the server
#
# The filters are separated into positive and negative (prepended by `!`). For
# an environment variable to be passed its name must match at least one positive
# filter (the default filter set is ["*"] which passes everything), and must not
# match any negative filters. The full glob syntax is described in the `glob`
# crate documentation <https://docs.rs/glob/latest/glob/struct.Pattern.html>,
# except the leading `!` for negative patterns which is parsed and removed by
# `lspmux`.
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
# emulator which would cause `lspmux` to spawn a new language server for every
# connected client, defeating its purpose.
#
# Example: pass_environment = ["*", "!DESKTOP_STARTUP_ID", "!WINDOWID", "!ALACRITTY_*"]
pass_environment = ["*"]
```


## Other LSP servers

By default `lspmux` uses a `rust-analyzer` binary found in its `$PATH` as the
server. This can be overridden using the `--server-path` cli option with the
client subcommand or `LSPMUX_SERVER` environment variable. You can usually
configure one of these in your editor configuration. If both are specified the
cli option overrides the environment variable.

For example with `coc-clangd` in CoC for neovim add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/home/user/.cargo/bin/lspmux",
    "clangd.arguments": ["client", "--server-path", "/usr/bin/clangd"]
}
```

Or to set a custom path for `rust-analyzer` with `coc-rust-analyzer` add to
`~/.config/nvim/coc-settings.json`:

```json
{
    "rust-analyzer.server.path": "/home/user/.cargo/bin/lspmux",
    "rust-analyzer.server.extraEnv": { "LSPMUX_SERVER": "/custom/path/rust-analyzer" }
}
```

If your editor configuration or plugin doesn't allow to add either you can
instead create a wrapper shell script and set it as the server path directly.
For example if `coc-clangd` didn't allow to pass additional arguments you'd
need a script like `/usr/local/bin/clangd-proxy`:

```sh
#!/bin/sh
exec /home/user/.cargo/bin/lspmux client --server-path /usr/bin/clangd $@
```

And configure the editor to use the wrapper script in
`~/.config/nvim/coc-settings.json`:

```json
{
    "clangd.path": "/usr/local/bin/clangd-proxy"
}
```
