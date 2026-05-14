// Entry point: parse args, load YAML, spawn PTYs + reader threads,
// install SIGHUP handler, run HTTP server on the main thread.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use tracing::{error, info};

mod capture;
mod config;
mod http;
mod logging;
mod matching;
mod port;
mod reload;
mod server;

use crate::port::PortState;
use crate::server::Server;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 || args[1] == "-h" || args[1] == "--help" {
        // Pre-logger: the user is invoking us wrong; write to stderr
        // directly so the message lands no matter what.
        eprintln!(
            "usage: {} <config.yaml>",
            args.first().map(String::as_str).unwrap_or("serial-mock-service")
        );
        return ExitCode::from(2);
    }

    let config_path = PathBuf::from(&args[1]);
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
