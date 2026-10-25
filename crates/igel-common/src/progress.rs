//! Progress events of the network phases, drawn by whoever installed a [`Sink`].

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub enum Event<'a> {
    IndexStarted {
        name: &'a str,
    },
    IndexFinished {
        name: &'a str,
    },
    DownloadsQueued {
        count: usize,
    },
    DownloadStarted {
        name: &'a str,
        version: &'a str,
    },
    DownloadBytes {
        bytes: u64,
    },
    DownloadFinished {
        name: &'a str,
        version: &'a str,
        ok: bool,
    },
}

pub trait Sink: Send + Sync {
    fn event(&self, event: Event<'_>);
}

static SINK: OnceLock<Box<dyn Sink>> = OnceLock::new();

pub fn set_sink(sink: Box<dyn Sink>) {
    let _ = SINK.set(sink);
}

pub fn emit(event: Event<'_>) {
    if let Some(sink) = SINK.get() {
        sink.event(event);
    }
}

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
