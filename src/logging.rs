use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Cap across all log files.
pub const MAX_TOTAL_BYTES: u64 = 10 * 1024 * 1024;
/// Log files kept: the live file plus rotated backups.
const KEEP_FILES: usize = 5;
/// Per-file cap; evenly divides the total.
const MAX_FILE_BYTES: u64 = MAX_TOTAL_BYTES / KEEP_FILES as u64;

/// Platform log directory: `LocalAppData` on Windows, the equivalent
/// application-data directory elsewhere.
#[must_use]
pub fn log_dir(app_id: &str) -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(app_id)
        .join("logs")
}

/// Route `log` records to stderr and a capped rotating file. Failures fall
/// back to stderr-only so logging never prevents startup. Returns the live
/// log file path for the startup record.
pub fn init(app_id: &str) -> PathBuf {
    let stem = app_id.rsplit('.').next().unwrap_or(app_id);
    let dir = log_dir(app_id);
    let current = dir.join(format!("{stem}.log"));
    match RotatingFile::open(&dir, stem) {
        Ok(file) => {
            let tee = Tee {
                file,
                stderr: io::stderr(),
            };
            let _ =
                env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                    .target(env_logger::Target::Pipe(Box::new(tee)))
                    .try_init();
        }
        Err(error) => {
            eprintln!(
                "siphon: file logging unavailable at {}: {error}",
                current.display()
            );
            env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
                .try_init()
                .ok();
        }
    }
    current
}

/// Fixed set of `stem.log` plus `stem.N.log` backups whose sizes sum to at
/// most the total cap. The oldest backup is discarded on rotation.
struct RotatingFile {
    dir: PathBuf,
    stem: String,
    max_file_bytes: u64,
    keep: usize,
    current: Option<File>,
    current_len: u64,
}

impl RotatingFile {
    fn open(dir: &Path, stem: &str) -> io::Result<Self> {
        fs::create_dir_all(dir)?;
        let mut file = Self {
            dir: dir.to_path_buf(),
            stem: stem.to_owned(),
            max_file_bytes: MAX_FILE_BYTES,
            keep: KEEP_FILES,
            current: None,
            current_len: 0,
        };
        let current = file.current_path();
        let opened = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&current)?;
        file.current_len = opened.metadata().map(|meta| meta.len()).unwrap_or(0);
        file.current = Some(opened);
        file.enforce_cap();
        Ok(file)
    }

    fn current_path(&self) -> PathBuf {
        self.dir.join(format!("{}.log", self.stem))
    }

    fn backup_path(&self, index: usize) -> PathBuf {
        self.dir.join(format!("{}.{index}.log", self.stem))
    }

    fn total_bytes(&self) -> u64 {
        let mut total = self.current_len;
        for index in 1..self.keep {
            total += fs::metadata(self.backup_path(index))
                .map(|meta| meta.len())
                .unwrap_or(0);
        }
        total
    }

    /// Delete oldest backups until the on-disk total fits the cap. A live
    /// file larger than the whole cap is truncated; rotation re-caps it.
    fn enforce_cap(&mut self) {
        self.enforce_cap_with(MAX_TOTAL_BYTES);
    }

    fn enforce_cap_with(&mut self, cap: u64) {
        while self.total_bytes() > cap {
            let mut removed = false;
            for index in (1..self.keep).rev() {
                if fs::remove_file(self.backup_path(index)).is_ok() {
                    removed = true;
                    break;
                }
            }
            if removed {
                continue;
            }
            if let Some(current) = self.current.as_ref()
                && current.set_len(0).is_ok()
            {
                self.current_len = 0;
            }
            break;
        }
    }

    /// The live handle is closed before renaming: renames of open files
    /// fail on Windows.
    fn rotate(&mut self) {
        drop(self.current.take());
        let _ = fs::remove_file(self.backup_path(self.keep - 1));
        for index in (1..self.keep - 1).rev() {
            let from = self.backup_path(index);
            if from.exists() {
                let _ = fs::rename(&from, self.backup_path(index + 1));
            }
        }
        let current = self.current_path();
        if current.exists() {
            let _ = fs::rename(&current, self.backup_path(1));
        }
        if let Ok(opened) = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&current)
        {
            self.current_len = 0;
            self.current = Some(opened);
        }
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.current_len + buf.len() as u64 > self.max_file_bytes {
            self.rotate();
        }
        match self.current.as_mut() {
            Some(current) => {
                current.write_all(buf)?;
                self.current_len += buf.len() as u64;
                Ok(buf.len())
            }
            None => Err(io::Error::other("log file unavailable")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.current.as_mut() {
            Some(current) => current.flush(),
            None => Ok(()),
        }
    }
}

/// `env_logger` pipe target writing each record to the file and stderr.
/// File errors are swallowed: stderr still carries the record.
struct Tee {
    file: RotatingFile,
    stderr: io::Stderr,
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.file.write_all(buf);
        self.stderr.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _ = self.file.flush();
        self.stderr.flush()
    }
}

/// RFC 3339 UTC timestamp.
pub fn timestamp() -> String {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let (year, month, day) = civil_from_days((elapsed.as_secs() / 86_400) as i64);
    let rem = elapsed.as_secs() % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60,
        elapsed.subsec_millis()
    )
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * i64::from(mp) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Milliseconds since the unix epoch for an ISO 8601 UTC timestamp like
/// `2026-09-10T18:35:06Z`; fractional seconds are accepted and truncated
/// to millisecond precision.
#[must_use]
pub fn parse_iso_ms(raw: &str) -> Option<i64> {
    let bytes = raw.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| raw.get(range)?.parse::<i64>().ok();
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    let millis = if bytes.len() > 19 && bytes[19] == b'.' {
        let end = bytes[20..]
            .iter()
            .position(|byte| !byte.is_ascii_digit())
            .map_or(bytes.len(), |offset| 20 + offset);
        let digits = raw.get(20..end)?;
        let leading = &digits[..digits.len().min(3)];
        let mut millis = leading.parse::<i64>().ok()?;
        for _ in leading.len()..3 {
            millis *= 10;
        }
        millis
    } else {
        0
    };
    Some(
        days_from_civil(year, month as u32, day as u32) * 86_400_000
            + hour * 3_600_000
            + minute * 60_000
            + second * 1000
            + millis,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("siphon-log-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_with(dir: &Path, keep: usize, max_file_bytes: u64) -> RotatingFile {
        let mut file = RotatingFile::open(dir, "siphon").unwrap();
        file.keep = keep;
        file.max_file_bytes = max_file_bytes;
        file
    }

    #[test]
    fn writes_rotate_and_drop_oldest_backup() {
        let dir = scratch_dir("rotate");
        let mut file = open_with(&dir, 3, 10);
        for _ in 0..10 {
            file.write_all(b"0123456789").unwrap();
        }
        drop(file);
        assert!(dir.join("siphon.log").exists());
        assert!(dir.join("siphon.1.log").exists());
        assert!(dir.join("siphon.2.log").exists());
        assert!(!dir.join("siphon.3.log").exists());
        let total: u64 = (0..3)
            .map(|index| {
                let path = if index == 0 {
                    dir.join("siphon.log")
                } else {
                    dir.join(format!("siphon.{index}.log"))
                };
                fs::metadata(path).map(|meta| meta.len()).unwrap_or(0)
            })
            .sum();
        assert!(total <= 3 * 10);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_cap_drops_oldest_first() {
        let dir = scratch_dir("cap");
        let mut file = open_with(&dir, 4, 1_000_000);
        drop(file.current.take());
        fs::write(dir.join("siphon.log"), [b'a'; 40]).unwrap();
        fs::write(dir.join("siphon.1.log"), [b'b'; 40]).unwrap();
        fs::write(dir.join("siphon.2.log"), [b'c'; 40]).unwrap();
        fs::write(dir.join("siphon.3.log"), [b'd'; 40]).unwrap();
        let mut file = open_with(&dir, 4, 1_000_000);
        drop(file.current.take());
        file.current_len = fs::metadata(dir.join("siphon.log"))
            .map(|meta| meta.len())
            .unwrap_or(0);
        file.enforce_cap_with(100);
        assert!(file.total_bytes() <= 100);
        assert!(!dir.join("siphon.3.log").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn enforce_cap_truncates_oversized_live_file() {
        let dir = scratch_dir("truncate");
        let mut file = open_with(&dir, 2, 1_000_000);
        drop(file.current.take());
        fs::write(dir.join("siphon.log"), [b'a'; 50]).unwrap();
        let mut file = open_with(&dir, 2, 1_000_000);
        drop(file.current.take());
        file.current = Some(
            OpenOptions::new()
                .write(true)
                .open(dir.join("siphon.log"))
                .unwrap(),
        );
        file.current_len = 50;
        file.enforce_cap_with(10);
        assert_eq!(file.current_len, 0);
        assert_eq!(
            fs::metadata(dir.join("siphon.log"))
                .map(|meta| meta.len())
                .unwrap_or(u64::MAX),
            0
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
