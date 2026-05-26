// Logging init.
//
// `tracing` is the logging facade; `tracing-subscriber` formats and
// writes the records. The YAML config picks the level + format
// (text or JSON); `RUST_LOG` overrides the level at runtime for
// debugging without editing config.

use tracing_subscriber::EnvFilter;

use crate::config::{LogFormat, LoggingConfig};

pub fn init(cfg: &LoggingConfig) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&cfg.level));

    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    match cfg.format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.json().init(),
    }
}
