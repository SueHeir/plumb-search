//! The app's log. `tracing` output goes to stderr, as with `plumb`, and to
//! `plumb.log` in the app's log folder, so that installed builds leave a log
//! to send with a bug report; on Windows they have no console at all. Each
//! start moves the previous run's log to `plumb.log.1`.

use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};

use plumb_node::DEFAULT_LOG_FILTER;
use tracing::{error, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

const LOG_FILE: &str = "plumb.log";
const PREVIOUS_LOG_FILE: &str = "plumb.log.1";

/// The most that is kept in memory for the log file before it opens.
const PENDING_LIMIT: usize = 64 * 1024;

static SINK: LogSink = LogSink::new();
static PATH: OnceLock<PathBuf> = OnceLock::new();

/// Sends `tracing` output, and panics, to stderr and to the log file,
/// filtered by `RUST_LOG` or [`DEFAULT_LOG_FILTER`] as with `plumb`. Until
/// [`open_file`] opens the file, its lines are kept in memory.
pub fn init() {
    let (filter, bad_spec) = match std::env::var("RUST_LOG") {
        Ok(spec) if !spec.trim().is_empty() => match EnvFilter::try_new(&spec) {
            Ok(filter) => (filter, None),
            Err(err) => (
                EnvFilter::new(DEFAULT_LOG_FILTER),
                Some(format!("ignoring RUST_LOG={spec:?}: {err}")),
            ),
        },
        _ => (EnvFilter::new(DEFAULT_LOG_FILTER), None),
    };
    let color =
        io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty());
    let installed = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(io::stderr).with_ansi(color))
        .with(fmt::layer().with_writer(|| FileWriter).with_ansi(false))
        .try_init();
    if installed.is_err() {
        return;
    }
    if let Some(bad_spec) = bad_spec {
        warn!("{bad_spec}");
    }
    let print_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        error!("{panic}");
        print_panic(panic);
    }));
}

/// Starts the log file `plumb.log` in `dir`, after moving the previous run's
/// log to `plumb.log.1`, and returns its path. When it cannot be opened, the
/// log goes to stderr only.
pub fn open_file(dir: &Path) -> io::Result<PathBuf> {
    let opened = start_file(dir).and_then(|(path, file)| {
        SINK.attach(file)?;
        Ok(path)
    });
    match opened {
        Ok(path) => {
            let _ = PATH.set(path.clone());
            Ok(path)
        }
        Err(err) => {
            SINK.close();
            Err(err)
        }
    }
}

/// The log file, once it is open.
pub fn file_path() -> Option<&'static Path> {
    PATH.get().map(PathBuf::as_path)
}

/// Creates an empty `plumb.log` in `dir`, keeping the one there as
/// `plumb.log.1`.
fn start_file(dir: &Path) -> io::Result<(PathBuf, File)> {
    fs::create_dir_all(dir)?;
    let path = dir.join(LOG_FILE);
    match fs::rename(&path, dir.join(PREVIOUS_LOG_FILE)) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        // Then the previous log is overwritten; not a reason to have none.
        Err(err) => warn!("cannot keep the previous log as {PREVIOUS_LOG_FILE}: {err}"),
    }
    let file = File::create(&path)?;
    Ok((path, file))
}

/// The log file's end of the log.
struct LogSink(Mutex<Sink>);

enum Sink {
    /// The file is not open yet; what was logged so far.
    Pending(Vec<u8>),
    File(File),
    /// There is no file.
    Closed,
}

impl LogSink {
    const fn new() -> Self {
        LogSink(Mutex::new(Sink::Pending(Vec::new())))
    }

    fn lock(&self) -> MutexGuard<'_, Sink> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes one formatted event, whole, so that events logged at the same
    /// time on several threads do not mix.
    fn write(&self, event: &[u8]) -> io::Result<()> {
        match &mut *self.lock() {
            Sink::Pending(lines) => {
                if lines.len() + event.len() <= PENDING_LIMIT {
                    lines.extend_from_slice(event);
                }
                Ok(())
            }
            Sink::File(file) => file.write_all(event),
            Sink::Closed => Ok(()),
        }
    }

    /// Writes what was logged so far to `file`, and everything after it.
    fn attach(&self, mut file: File) -> io::Result<()> {
        let mut sink = self.lock();
        if let Sink::Pending(lines) = &*sink {
            file.write_all(lines)?;
        }
        *sink = Sink::File(file);
        Ok(())
    }

    /// Stops keeping lines for a file that did not open.
    fn close(&self) {
        *self.lock() = Sink::Closed;
    }
}

/// The writer `tracing` formats each event into for the log file.
struct FileWriter;

impl Write for FileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        SINK.write(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory for one test.
    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("plumb-desktop-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn each_start_keeps_the_previous_log() {
        let dir = test_dir("rotate").join("logs");
        for run in ["first", "second", "third"] {
            let (path, mut file) = start_file(&dir).unwrap();
            assert_eq!(path, dir.join("plumb.log"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "");
            file.write_all(run.as_bytes()).unwrap();
        }
        assert_eq!(fs::read_to_string(dir.join("plumb.log")).unwrap(), "third");
        assert_eq!(
            fs::read_to_string(dir.join("plumb.log.1")).unwrap(),
            "second"
        );
        fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn lines_logged_before_the_file_opens_go_into_it_first() {
        let dir = test_dir("pending");
        let sink = LogSink::new();
        sink.write(b"early\n").unwrap();
        let (path, file) = start_file(&dir).unwrap();
        sink.attach(file).unwrap();
        sink.write(b"later\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "early\nlater\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn little_is_kept_for_a_file_that_never_opens() {
        let sink = LogSink::new();
        let line = [b'x'; 1000];
        for _ in 0..100 {
            sink.write(&line).unwrap();
        }
        match &*sink.lock() {
            Sink::Pending(lines) => assert!(lines.len() <= PENDING_LIMIT),
            _ => unreachable!(),
        }
        sink.close();
        sink.write(&line).unwrap();
        assert!(matches!(&*sink.lock(), Sink::Closed));
    }
}
