// SIGHUP-driven config reload.
//
// Re-reads the YAML, validates it, and applies it to the running set:
//   - existing ports get scenarios/rules swapped in place (PTY stays
//     open, capture buffers preserved)
//   - new ports get fresh PTYs spawned (paths printed)
//   - removed ports are dropped (their reader thread exits when the
//     master fd is closed)
//
// On any validation error, the previous config is kept intact.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use signal_hook::consts::SIGHUP;
use signal_hook::iterator::Signals;

use crate::config::{self, Config};
use crate::port::PortState;
use crate::server::Server;

pub fn install(config_path: PathBuf, server: Arc<Server>) -> std::io::Result<()> {
    let mut signals = Signals::new([SIGHUP])?;
    thread::Builder::new()
        .name("sighup-reload".into())
        .spawn(move || {
            for _ in signals.forever() {
                println!("SIGHUP: reloading {}", config_path.display());
                match config::load(&config_path) {
                    Ok(new_cfg) => apply(&server, new_cfg),
                    Err(e) => eprintln!("reload aborted: {}", e),
                }
            }
        })?;
    Ok(())
}

fn apply(server: &Arc<Server>, new_cfg: Config) {
    let existing: Vec<String> = server.ports.names();
    let new_names: Vec<String> = new_cfg.ports.iter().map(|p| p.name.clone()).collect();

    // Update or spawn each port in the new config.
    for port_cfg in &new_cfg.ports {
        if let Some(state) = server.ports.get(&port_cfg.name) {
            if let Err(e) = state.swap_config(port_cfg) {
                eprintln!("port {}: reload swap failed: {}", port_cfg.name, e);
            } else {
                println!("port {}: config reloaded", port_cfg.name);
            }
        } else {
            match PortState::spawn(port_cfg) {
                Ok(state) => {
                    println!(
                        "port {}: spawned (slave: {})",
                        state.name,
                        state.pty_path.display()
                    );
                    server.ports.insert(state);
                }
                Err(e) => eprintln!("port {}: spawn on reload failed: {}", port_cfg.name, e),
            }
        }
    }

    // Drop ports no longer present.
    for name in existing {
        if !new_names.iter().any(|n| n == &name) {
            if let Some(_state) = server.ports.remove(&name) {
                println!("port {}: removed", name);
                // Dropping `_state` here closes the master File once
                // all Arcs go out of scope; reader thread exits on EIO.
            }
        }
    }
}
