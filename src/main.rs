use std::env;

use anyhow::Result;
use clap::{Parser, Subcommand};
use lsp_herd::config::Config;
use lsp_herd::{ext, proxy, server};
use tracing::info;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// No command defaults to client
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Connect to an lsp-herd server [default]
    Client {
        /// Path to the LSP server executable
        #[arg(
            long = "server-path",
            env = "LSP_HERD_SERVER",
            default_value = "rust-analyzer",
            name = "SERVER_PATH"
        )]
        server: String,

        /// Arguments passed to the LSP server
        #[arg(name = "SERVER_ARGS")]
        args: Vec<String>,
    },

    /// Start an lsp-herd server
    Server {},

    /// Print server status
    Status {
        /// Output data as machine readable JSON
        #[clap(long = "json", default_value = "false")]
        json: bool,

        /// Output more information
        #[clap(short = 'v', long = "verbose", default_value = "false")]
        verbose: bool,
    },

    /// Print server configuration
    Config {},

    /// Reload workspace
    ///
    /// For rust-analyzer send the `rust-analyzer/reloadWorkspace` extension request.
    /// Do nothing for other language servers.
    Reload {},

    /// Reload server file state from disk
    ///
    /// Make sure to save in all your open editors before running this command,
    /// or the state will immediately desync again.
    ///
    /// Useful when multiple clients have opened the same file and have sent
    /// conflicting edits to the server resulting in an inconstistent file
    /// state, this often manifests as persistent syntax errors thrown by the
    /// server.
    Sync {},
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let config = match Config::try_load() {
        Ok(config) => {
            config.init_logger();
            config
        }
        Err(err) => {
            let config = Config::default();
            config.init_logger();
            // Log only after the logger has been initialized
            info!(?err, "cannot load config file, continuing with defaults");
            config
        }
    };

    match cli.command {
        Some(Cmd::Server {}) => server::run(&config).await,
        Some(Cmd::Client { server, args }) => proxy::run(&config, server, args).await,
        Some(Cmd::Status { json, verbose }) => ext::status(&config, json, verbose).await,
        Some(Cmd::Config {}) => ext::config(&config),
        Some(Cmd::Reload {}) => ext::reload(&config).await,
        Some(Cmd::Sync {}) => ext::sync(&config).await,
        None => {
            let server_path =
                env::var("LSP_HERD_SERVER").unwrap_or_else(|_| "rust-analyzer".into());
            proxy::run(&config, server_path, vec![]).await
        }
    }
}
