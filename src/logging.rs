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
        remove_incomplete_copies(directory, &cfg.filename);
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

// Suffixes of the files that `reopen_compressed_log` and `adopt_next_day_log`
// write a day's file into before renaming it into place.
const RESTORING: &str = "restoring";
const MERGING: &str = "merging";

/// Remove the files a startup interrupted while writing a copy left behind.
/// A copy is renamed into place as soon as it is complete, and its sources are
/// kept until then, so any copy still present is incomplete and redundant.
/// Neither the appender nor its retention limit touches these files.
fn remove_incomplete_copies(directory: &Path, filename: &str) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let prefix = format!("{filename}.");
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(rest) = name.to_str().and_then(|n| n.strip_prefix(&prefix)) else {
            continue;
        };
        let is_copy = rest.split_once('.').is_some_and(|(day, suffix)| {
            (suffix == RESTORING || suffix == MERGING)
                && day.len() == 10
                && day.bytes().all(|b| b.is_ascii_digit() || b == b'-')
        });
        if is_copy {
            let _ = fs::remove_file(entry.path());
        }
    }
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

    let partial = directory.join(format!("{filename}.{day}.{RESTORING}"));
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
///
/// The combined file is written beside the current one and renamed over it,
/// so an interruption leaves either the original files or the combined one.
/// If the next day's file outlives a completed merge, its first entry is
/// already in the current file, and it is only removed.
fn adopt_next_day_log(
    directory: &Path,
    filename: &str,
    day: &str,
    next_day: &str,
) -> io::Result<Option<PathBuf>> {
    let next = directory.join(format!("{filename}.{next_day}"));
    let Some(first_entry) = first_line(&next)? else {
        return Ok(None);
    };
    let stamped_today = serde_json::from_str::<serde_json::Value>(&first_entry)
        .ok()
        .and_then(|entry| Some(entry.get("timestamp")?.as_str()?.starts_with(day)))
        .unwrap_or(false);
    if !stamped_today {
        return Ok(None);
    }

    let current = directory.join(format!("{filename}.{day}"));
    if !contains_line(&current, &first_entry)? {
        let merging = directory.join(format!("{filename}.{day}.{MERGING}"));
        let merge = || -> io::Result<()> {
            let mut output = fs::File::create(&merging)?;
            match fs::File::open(&current) {
                Ok(mut existing) => {
                    io::copy(&mut existing, &mut output)?;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
            io::copy(&mut fs::File::open(&next)?, &mut output)?;
            output.sync_all()?;
            fs::rename(&merging, &current)
        };
        if let Err(e) = merge() {
            let _ = fs::remove_file(&merging);
            return Err(e);
        }
    }
    fs::remove_file(&next)?;
    Ok(Some(next))
}

/// The first line of the file at `path` without its line ending, or `None`
/// if the file does not exist.
fn first_line(path: &Path) -> io::Result<Option<String>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut line = String::new();
    io::BufReader::new(file).read_line(&mut line)?;
    Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()))
}

/// Whether the file at `path` exists and has a line equal to `line`.
fn contains_line(path: &Path, line: &str) -> io::Result<bool> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    for candidate in io::BufReader::new(file).split(b'\n') {
        let candidate = candidate?;
        if candidate.strip_suffix(b"\r").unwrap_or(&candidate) == line.as_bytes() {
            return Ok(true);
        }
    }
    Ok(false)
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
    fn finishes_a_merge_interrupted_before_the_next_days_file_was_removed() {
        // The current file already holds the next day's entries, as after a
        // merge that stopped before removing the next day's file.
        let dir = scratch_dir("adopt-resume");
        let merged = format!("earlier\n{NEXT_DAY_ENTRY}\nlater\n");
        fs::write(dir.join("proxy.log.2026-01-02"), &merged).unwrap();
        fs::write(
            dir.join("proxy.log.2026-01-03"),
            format!("{NEXT_DAY_ENTRY}\nlater\n"),
        )
        .unwrap();

        let moved = adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap();

        assert_eq!(moved, Some(dir.join("proxy.log.2026-01-03")));
        assert_eq!(
            fs::read_to_string(dir.join("proxy.log.2026-01-02")).unwrap(),
            merged
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn replaces_a_stale_partial_merge() {
        let dir = scratch_dir("adopt-stale-merging");
        fs::write(dir.join("proxy.log.2026-01-02"), "earlier\n").unwrap();
        fs::write(
            dir.join("proxy.log.2026-01-03"),
            format!("{NEXT_DAY_ENTRY}\n"),
        )
        .unwrap();
        fs::write(dir.join("proxy.log.2026-01-02.merging"), "partial").unwrap();

        adopt_next_day_log(&dir, "proxy.log", "2026-01-02", "2026-01-03").unwrap();

        assert_eq!(
            fs::read_to_string(dir.join("proxy.log.2026-01-02")).unwrap(),
            format!("earlier\n{NEXT_DAY_ENTRY}\n")
        );
        assert_eq!(entries(&dir), ["proxy.log.2026-01-02"]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removes_incomplete_copies_only() {
        let dir = scratch_dir("incomplete-copies");
        for name in [
            "proxy.log.2026-01-01.restoring",
            "proxy.log.2026-01-02.merging",
            "proxy.log.2026-01-02",
            "proxy.log.2026-01-01.gz",
            "proxy.log.notes.merging",
            "other.log.2026-01-02.merging",
        ] {
            fs::write(dir.join(name), "x").unwrap();
        }

        remove_incomplete_copies(&dir, "proxy.log");

        assert_eq!(
            entries(&dir),
            [
                "other.log.2026-01-02.merging",
                "proxy.log.2026-01-01.gz",
                "proxy.log.2026-01-02",
                "proxy.log.notes.merging",
            ]
        );
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
