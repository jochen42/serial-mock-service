// Entry point.
//
// Dispatches between `serve` (long-running daemon) and the various
// client subcommands defined in `cli::Cmd`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use tracing::{error, info};

mod capture;
mod cli;
mod config;
mod http;
mod logging;
mod matching;
mod port;
mod reload;
mod server;

use crate::cli::{Cli, Cmd};
use crate::port::PortState;
use crate::server::Server;

fn main() -> ExitCode {
    let parsed = Cli::parse();

    match parsed.command {
        Cmd::Serve { config } => serve(config),
        other => ExitCode::from(cli::run(other) as u8),
    }
}

fn serve(config_path: PathBuf) -> ExitCode {
    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            // Logger not initialised yet — fall back to stderr.
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    logging::init(&cfg.logging);

    info!("serial-mock-service starting");

    let server = Arc::new(Server {
        ports: server::PortMap::new(),
    });

    for port_cfg in &cfg.ports {
        match PortState::spawn(port_cfg) {
            Ok(state) => {
                info!(
                    port = %state.name,
                    slave = %state.pty_path.display(),
                    initial_scenario = %port_cfg.initial_scenario,
                    "port {}: slave = {}",
                    state.name,
                    state.pty_path.display(),
                );
                server.ports.insert(state);
            }
            Err(e) => {
                error!(port = %port_cfg.name, error = %e, "port spawn failed");
                return ExitCode::from(1);
            }
        }
    }

    if let Err(e) = reload::install(config_path.clone(), server.clone()) {
        error!(error = %e, "failed to install SIGHUP handler");
        return ExitCode::from(1);
    }
    info!(config = %config_path.display(), "SIGHUP will reload from this path");

    if let Err(e) = http::serve(&cfg.http.bind, server) {
        error!(error = %e, "http server failed");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
