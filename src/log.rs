// Copyright (C) 2026 Jörg Schröder
// SPDX-License-Identifier: GPL-3.0-or-later

//! Persistent debug logging — every trace line goes to stderr AND
//! to `~/.local/state/odin/odin.log`, so a crash or a
//! timeout-kill can never eat the last diagnostics. Debugging is a
//! first-class contract: errors must be localizable without guessing.

use std::fs::{self, File};
use std::io::{self, Write};
use tracing_subscriber::fmt::MakeWriter;

/// Tee-writer: duplicates each log record into stderr + the log file.
struct Tee {
    stderr: io::Stderr,
    file: File,
}
impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _ = self.stderr.write_all(buf);
        let n = self.file.write(buf)?;
        // A GPU wedge + hard reset eats dirty page-cache lines —
        // every marker we write before a hanging ioctl must hit the
        // platter, or the crash location is unrecoverable.
        let _ = self.file.sync_data();
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        let _ = self.stderr.flush();
        let r = self.file.flush();
        let _ = self.file.sync_data();
        r
    }
}

/// `MakeWriter` producing one Tee per record.
struct MakeTee {
    file: std::sync::Arc<std::sync::Mutex<File>>,
}
impl<'a> MakeWriter<'a> for MakeTee {
    type Writer = Tee;
    fn make_writer(&'a self) -> Self::Writer {
        // Lock-free fast path is overkill at log rates — serialize
        // on the file handle so records never interleave.
        Tee {
            stderr: io::stderr(),
            file: self.file.lock().unwrap().try_clone().unwrap(),
        }
    }
}

/// Install the global subscriber. Returns the log path so the
/// caller can print it at startup.
pub fn init() -> std::path::PathBuf {
    let dir = dirs_state();
    fs::create_dir_all(&dir).ok();
    let path = dir.join("odin.log");
    let file = File::options()
        .create(true)
        .append(true)
        .open(&path)
        .expect("cannot open odin.log");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "odin=info".into()),
        )
        .with_writer(MakeTee {
            file: std::sync::Arc::new(std::sync::Mutex::new(file)),
        })
        .init();
    path
}

/// XDG state dir for the compositor — matches the old project's
/// layout so tools know where to look.
fn dirs_state() -> std::path::PathBuf {
    // Empty-but-set vars are as bad as unset — filter both.
    let xdg = std::env::var("XDG_STATE_HOME").unwrap_or_default();
    let base = if !xdg.is_empty() {
        std::path::PathBuf::from(xdg)
    } else {
        std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".local/state")
    };
    base.join("odin")
}
