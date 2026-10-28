//! Raw frame recordings, for replaying a live session through the bar
//! builder (and for regression tests built from real Angel traffic).
//!
//! Format: the 8-byte magic `SWS2REC1`, then per frame a little-endian
//! `u64` receive time in Unix microseconds, a `u32` length and the frame
//! bytes. A truncated last record (a crash mid-write) is ignored on read.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::Path;

use chrono::{DateTime, Utc};
use saarthi_core::Ts;

pub const MAGIC: &[u8; 8] = b"SWS2REC1";

pub struct Recorder {
    out: BufWriter<File>,
    frames: u64,
}

impl Recorder {
    /// Creates `path`, or appends to it if it already holds a recording (an
    /// engine restarted mid-session keeps one file per day).
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let exists = path.metadata().map(|m| m.len() > 0).unwrap_or(false);
        if exists {
            let mut magic = [0u8; 8];
            File::open(path)?.read_exact(&mut magic)?;
            if &magic != MAGIC {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "not a frame recording"));
            }
        }
        let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let mut out = BufWriter::with_capacity(1 << 16, f);
        if !exists {
            out.write_all(MAGIC)?;
        }
        Ok(Recorder { out, frames: 0 })
    }

    pub fn write(&mut self, received: Ts, frame: &[u8]) -> io::Result<()> {
        let micros = received.timestamp_micros().max(0) as u64;
        let len = u32::try_from(frame.len()).map_err(|_| io::Error::other("frame too large"))?;
        self.out.write_all(&micros.to_le_bytes())?;
        self.out.write_all(&len.to_le_bytes())?;
        self.out.write_all(frame)?;
        self.frames += 1;
        // Bound what a crash can lose without a syscall per frame.
        if self.frames % 256 == 0 {
            self.out.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.out.flush();
    }
}

/// Reads every complete frame of a recording.
pub fn read_frames(path: &Path) -> io::Result<Vec<(Ts, Vec<u8>)>> {
    let mut r = BufReader::new(File::open(path)?);
    let mut magic = [0u8; 8];
    r.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a frame recording"));
    }
    let mut frames = Vec::new();
    loop {
        let mut head = [0u8; 12];
        match r.read_exact(&mut head) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let micros = u64::from_le_bytes(head[..8].try_into().expect("8 bytes"));
        let len = u32::from_le_bytes(head[8..].try_into().expect("4 bytes")) as usize;
        let mut buf = vec![0u8; len];
        match r.read_exact(&mut buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                tracing::warn!(path = %path.display(), "recording ends in a truncated frame; ignoring it");
                break;
            }
            Err(e) => return Err(e),
        }
        let ts = DateTime::<Utc>::from_timestamp_micros(micros as i64).unwrap_or_default();
        frames.push((ts, buf));
    }
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_appends_and_tolerates_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("day/frames.bin");
        let t = DateTime::<Utc>::from_timestamp_micros(1_793_072_700_250_123).unwrap();
        {
            let mut r = Recorder::open(&path).unwrap();
            r.write(t, b"abc").unwrap();
        }
        {
            let mut r = Recorder::open(&path).unwrap();
            r.write(t, b"defg").unwrap();
        }
        let frames = read_frames(&path).unwrap();
        assert_eq!(frames, vec![(t, b"abc".to_vec()), (t, b"defg".to_vec())]);

        let mut bytes = std::fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 2);
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_frames(&path).unwrap().len(), 1);

        std::fs::write(&path, b"garbage!").unwrap();
        assert!(Recorder::open(&path).is_err());
        assert!(read_frames(&path).is_err());
    }
}
