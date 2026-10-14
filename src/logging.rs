use crate::config::LoggingConfig;
use logroller::{Compression, LogRollerBuilder, Rotation, RotationAge, TimeZone};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init(file_config: Option<&LoggingConfig>) -> anyhow::Result<()> {
    let env_filter = EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()));
    let console_layer = tracing_subscriber::fmt::layer().json();

    if let Some(cfg) = file_config.filter(|c| c.enabled) {
        std::fs::create_dir_all(&cfg.directory)?;

        // Reported once logging is up; a failure must not keep the node from
        // starting.
        let reopened = cfg.compression.then(|| {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            reopen_compressed_log(Path::new(&cfg.directory), &cfg.filename, &today)
        });

        // Daily files follow UTC days, matching the UTC timestamps of the
        // entries they hold. logroller's default is the host's local offset,
        // captured once at startup.
        let mut builder = LogRollerBuilder::new(&cfg.directory, &cfg.filename)
            .rotation(Rotation::AgeBased(RotationAge::Daily))
            .time_zone(TimeZone::UTC)
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

        match reopened {
            Some(Ok(Some(path))) => tracing::info!(
                path = %path.display(),
                "Restored today's compressed log file to continue writing it"
            ),
            Some(Err(e)) => tracing::warn!(
                error = %e,
                "Failed to restore today's compressed log file; the next rotation will overwrite it"
            ),
            _ => {}
        }
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(console_layer)
            .init();
    }

    Ok(())
}

/// Decompress `<filename>.<day>.gz` back to `<filename>.<day>` when only the
/// compressed file exists, so that the day's file is continued. Returns the
/// restored path, or `None` if there was nothing to restore.
///
/// logroller compresses a file when it rotates and overwrites any compressed
/// file of the same name. A day that was already rotated is reopened when the
/// node starts with its clock behind the last rotation, and once when upgrading
/// from a version that rotated on local days on a host east of UTC. Without
/// this, the day's earlier entries would be lost at the next rotation.
fn reopen_compressed_log(
    directory: &Path,
    filename: &str,
    day: &str,
) -> io::Result<Option<PathBuf>> {
    let current = directory.join(format!("{filename}.{day}"));
    let compressed = directory.join(format!("{filename}.{day}.gz"));
    if current.exists() || !compressed.exists() {
        return Ok(None);
    }

    let partial = directory.join(format!("{filename}.{day}.restoring"));
    let restore = || -> io::Result<()> {
        let mut decoder =
            flate2::read::MultiGzDecoder::new(io::BufReader::new(fs::File::open(&compressed)?));
        let mut output = fs::File::create(&partial)?;
        io::copy(&mut decoder, &mut output)?;
        output.sync_all()?;
        fs::rename(&partial, &current)
    };
    if let Err(e) = restore() {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    // The entries are now in `current`. Should removal fail, the next rotation
    // overwrites the leftover with `current`, which holds the same entries.
    let _ = fs::remove_file(&compressed);
    Ok(Some(current))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// An empty scratch directory unique to this test.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("llamesh-logging-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_gzip(path: &Path, members: &[&str]) {
        let mut file = fs::File::create(path).unwrap();
        for member in members {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(member.as_bytes()).unwrap();
            file.write_all(&encoder.finish().unwrap()).unwrap();
        }
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn restores_a_compressed_file_for_the_current_day() {
        let dir = scratch_dir("restores");
        // logroller writes one gzip member; accept concatenated members too.
        write_gzip(
            &dir.join("proxy.log.2026-01-02.gz"),
            &["first\n", "second\n"],
        );

        let restored = reopen_compressed_log(&dir, "proxy.log", "2026-01-02").unwrap();

        assert_eq!(restored, Some(dir.join("proxy.log.2026-01-02")));
        assert_eq!(
            fs::read_to_string(dir.join("proxy.log.2026-01-02")).unwrap(),
            "first\nsecond\n"
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leaves_files_alone_when_the_current_file_exists() {
        let dir = scratch_dir("current-exists");
        write_gzip(&dir.join("proxy.log.2026-01-02.gz"), &["old\n"]);
        fs::write(dir.join("proxy.log.2026-01-02"), "current\n").unwrap();

        assert_eq!(
            reopen_compressed_log(&dir, "proxy.log", "2026-01-02").unwrap(),
            None
        );
        assert_eq!(
            fs::read_to_string(dir.join("proxy.log.2026-01-02")).unwrap(),
            "current\n"
        );
        assert_eq!(
            entries(&dir),
            ["proxy.log.2026-01-02", "proxy.log.2026-01-02.gz"]
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ignores_other_days() {
        let dir = scratch_dir("other-days");
        write_gzip(&dir.join("proxy.log.2026-01-01.gz"), &["yesterday\n"]);

        assert_eq!(
            reopen_compressed_log(&dir, "proxy.log", "2026-01-02").unwrap(),
            None
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-01.gz"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keeps_a_compressed_file_it_cannot_read() {
        let dir = scratch_dir("corrupt");
        fs::write(dir.join("proxy.log.2026-01-02.gz"), "not gzip").unwrap();

        assert!(reopen_compressed_log(&dir, "proxy.log", "2026-01-02").is_err());
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02.gz"]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
