//! Progress of the network phases — index lookups and crate downloads —
//! reported by the libraries that do them and drawn by whoever installed a
//! [`Sink`] (the CLI; nothing is drawn in tests or when used as a library).
//!
//! Events may come from many threads at once (fetches run in parallel).

use std::sync::OnceLock;

/// Something that happened in a network phase.
#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    /// An index file is being fetched over the network.
    IndexStarted { name: &'a str },
    /// That fetch finished (successfully or not; errors are reported by the
    /// caller that needs the crate).
    IndexFinished { name: &'a str },
    /// `count` crate archives are about to be downloaded.
    DownloadsQueued { count: usize },
    /// A crate archive started downloading.
    DownloadStarted { name: &'a str, version: &'a str },
    /// `bytes` more bytes of some archive arrived.
    DownloadBytes { bytes: u64 },
    /// An archive's download ended: fetched (`ok`) or failed.
    DownloadFinished {
        name: &'a str,
        version: &'a str,
        ok: bool,
    },
}

/// Receives [`Event`]s.
pub trait Sink: Send + Sync {
    fn event(&self, event: Event<'_>);
}

static SINK: OnceLock<Box<dyn Sink>> = OnceLock::new();

/// Installs the process-wide sink. The first call wins.
pub fn set_sink(sink: Box<dyn Sink>) {
    let _ = SINK.set(sink);
}

/// Reports `event` to the sink, if one is installed.
pub fn emit(event: Event<'_>) {
    if let Some(sink) = SINK.get() {
        sink.event(event);
    }
}

/// A reader that reports every chunk it yields as [`Event::DownloadBytes`].
pub struct CountingReader<R>(pub R);

impl<R: std::io::Read> std::io::Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        if n > 0 {
            emit(Event::DownloadBytes { bytes: n as u64 });
        }
        Ok(n)
    }
}
