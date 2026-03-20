use crate::config::LoggingConfig;
use logroller::{Compression, LogRollerBuilder, Rotation, RotationAge};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init(file_config: Option<&LoggingConfig>) -> anyhow::Result<()> {
    let env_filter = EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()));
    let console_layer = tracing_subscriber::fmt::layer().json();

    if let Some(cfg) = file_config.filter(|c| c.enabled) {
        std::fs::create_dir_all(&cfg.directory)?;

        let mut builder = LogRollerBuilder::new(&cfg.directory, &cfg.filename)
            .rotation(Rotation::AgeBased(RotationAge::Daily))
            .max_keep_files(cfg.max_keep_files);

        if cfg.compression {
            builder = builder.compression(Compression::Gzip);
        }

        let appender = builder.build()?;
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        Box::leak(Box::new(guard)); // Keep alive for program lifetime

        let file_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(non_blocking);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .init();
    }

    Ok(())
}
