use crate::config::LoggingConfig;
use logroller::{Compression, LogRollerBuilder, Rotation, RotationAge, TimeZone};
use std::fs;
use std::io::{self, BufRead};
use std::path::{Path, PathBuf};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

pub fn init(file_config: Option<&LoggingConfig>) -> anyhow::Result<()> {
    let env_filter = EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()));
    let console_layer = tracing_subscriber::fmt::layer().json();

    if let Some(cfg) = file_config.filter(|c| c.enabled) {
        std::fs::create_dir_all(&cfg.directory)?;

        // Prepare the current day's file before the appender opens it. The
        // outcomes are reported once logging is up; a failure must not keep
        // the node from starting.
        let today = chrono::Utc::now().date_naive();
        let next_day = (today + chrono::Days::new(1))
            .format("%Y-%m-%d")
            .to_string();
        let today = today.format("%Y-%m-%d").to_string();
        let directory = Path::new(&cfg.directory);
        let reopened = cfg
            .compression
            .then(|| reopen_compressed_log(directory, &cfg.filename, &today));
        let adopted = adopt_next_day_log(directory, &cfg.filename, &today, &next_day);

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
        match adopted {
            Ok(Some(path)) => tracing::info!(
                path = %path.display(),
                "Moved entries stamped today from the next day's log file into today's"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "Failed to move entries stamped today out of the next day's log file"
            ),
            Ok(None) => {}
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

/// Move the entries of `<filename>.<next_day>` to the end of
/// `<filename>.<day>` when that file's first entry is stamped on `day`.
/// Returns the path the entries were moved from, or `None` if there was
/// nothing to move.
///
/// A version that rotated on local days named its file after the local date,
/// which east of UTC is already the next UTC day between local and UTC
/// midnight. Upgrading in that window would leave that interval's entries in
/// the next day's file, which the appender opens at the next rotation and
/// appends the next day's entries to. A next-day file whose entries are
/// stamped on the next day, written before the clock was set back, is left
/// alone.
fn adopt_next_day_log(
    directory: &Path,
    filename: &str,
    day: &str,
    next_day: &str,
) -> io::Result<Option<PathBuf>> {
    let next = directory.join(format!("{filename}.{next_day}"));
    let file = match fs::File::open(&next) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut first_entry = String::new();
    io::BufReader::new(file).read_line(&mut first_entry)?;
    let stamped_today = serde_json::from_str::<serde_json::Value>(&first_entry)
        .ok()
        .and_then(|entry| Some(entry.get("timestamp")?.as_str()?.starts_with(day)))
        .unwrap_or(false);
    if !stamped_today {
        return Ok(None);
    }

    // Take the file out of the appender's way first, so that an interrupted
    // move leaves the entries in this file instead of copying them twice.
    let moving = directory.join(format!("{filename}.{next_day}.moving"));
    fs::rename(&next, &moving)?;
    let append = || -> io::Result<()> {
        let mut current = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join(format!("{filename}.{day}")))?;
        io::copy(&mut fs::File::open(&moving)?, &mut current)?;
        current.sync_all()
    };
    append().map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("{e} (the entries remain in {})", moving.display()),
        )
    })?;
    let _ = fs::remove_file(&moving);
    Ok(Some(next))
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

    const NEXT_DAY_ENTRY: &str = r#"{"timestamp":"2026-01-02T23:00:00.000000Z","level":"INFO"}"#;

    #[test]
    fn moves_entries_stamped_today_out_of_the_next_days_file() {
        let dir = scratch_dir("adopt");
        fs::write(dir.join("proxy.log.2026-01-02"), "earlier\n").unwrap();
        fs::write(
            dir.join("proxy.log.2026-01-03"),
            format!("{NEXT_DAY_ENTRY}\nlater\n"),
        )
        .unwrap();

        let moved = adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap();

        assert_eq!(moved, Some(dir.join("proxy.log.2026-01-03")));
        assert_eq!(
            fs::read_to_string(dir.join("proxy.log.2026-01-02")).unwrap(),
            format!("earlier\n{NEXT_DAY_ENTRY}\nlater\n")
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn moves_entries_into_a_new_file_for_today() {
        let dir = scratch_dir("adopt-new");
        fs::write(
            dir.join("proxy.log.2026-01-03"),
            format!("{NEXT_DAY_ENTRY}\n"),
        )
        .unwrap();

        adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("proxy.log.2026-01-02")).unwrap(),
            format!("{NEXT_DAY_ENTRY}\n")
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leaves_a_next_day_file_stamped_on_its_own_day() {
        // Written while the clock was ahead of where it is now.
        let dir = scratch_dir("adopt-own-day");
        let own_day = r#"{"timestamp":"2026-01-03T01:00:00.000000Z","level":"INFO"}"#;
        fs::write(dir.join("proxy.log.2026-01-03"), format!("{own_day}\n")).unwrap();

        assert_eq!(
            adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap(),
            None
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-03"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn leaves_a_next_day_file_it_cannot_date() {
        let dir = scratch_dir("adopt-undated");
        fs::write(dir.join("proxy.log.2026-01-03"), "not json\n").unwrap();

        assert_eq!(
            adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap(),
            None
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-03"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn nothing_to_move_without_a_next_day_file() {
        let dir = scratch_dir("adopt-absent");
        fs::write(dir.join("proxy.log.2026-01-02"), "earlier\n").unwrap();

        assert_eq!(
            adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap(),
            None
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02"]);
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
