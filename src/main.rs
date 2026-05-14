// Entry point: parse args, load YAML, spawn PTYs + reader threads,
// install SIGHUP handler, run HTTP server on the main thread.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

mod capture;
mod config;
mod http;
mod matching;
mod port;
mod reload;
mod server;

use crate::port::PortState;
use crate::server::Server;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 2 || args[1] == "-h" || args[1] == "--help" {
        eprintln!("usage: {} <config.yaml>", args.first().map(String::as_str).unwrap_or("serial-mock-service"));
        return ExitCode::from(2);
    }

    let config_path = PathBuf::from(&args[1]);
    let cfg = match config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {}", e);
            return ExitCode::from(1);
        }
    };

    let server = Arc::new(Server { ports: server::PortMap::new() });

    println!("== serial-mock-service ==");
    for port_cfg in &cfg.ports {
        match PortState::spawn(port_cfg) {
            Ok(state) => {
                println!(
                    "port {}: slave = {}  (initial scenario: {})",
                    state.name,
                    state.pty_path.display(),
                    port_cfg.initial_scenario,
                );
                server.ports.insert(state);
            }
            Err(e) => {
                eprintln!("port {}: spawn failed: {}", port_cfg.name, e);
                return ExitCode::from(1);
            }
        }
    }

    if let Err(e) = reload::install(config_path.clone(), server.clone()) {
        eprintln!("failed to install SIGHUP handler: {}", e);
        return ExitCode::from(1);
    }
    println!("SIGHUP: reload config from {}", config_path.display());

    if let Err(e) = http::serve(&cfg.http.bind, server) {
        eprintln!("http server: {}", e);
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
