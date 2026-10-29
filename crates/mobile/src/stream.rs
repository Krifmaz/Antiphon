//! Reading a picked file a window at a time.
//!
//! A phone hands the app a picked file as a handle, not a path, and every
//! read through that handle is a trip through the platform (a content
//! resolver on Android, a bookmark on iOS). The transfer asks for one
//! 3800-byte chunk at a time, so going to the platform for each would cost
//! hundreds of thousands of trips for a large video. [`Windowed`] asks for
//! a megabyte at a time instead and answers the chunk reads from that, so
//! the file is never held whole and the trips stay few.
//!
//! Generic over the read function, so the windowing is testable without a
//! phone or a file. Plain std, so its tests run on the host.

use std::io::{self, Read};
use std::sync::Mutex;

/// How much is asked of the platform at once: big enough that the trips
/// are few, small enough that a send holds only this much of the file.
pub const WINDOW: usize = 1 << 20;

/// A file of known length read through `read(offset, len)`, which returns
/// up to `len` bytes from `offset` (fewer only at the end of the file).
///
/// Keeps the last window read, so sequential chunk reads cost one platform
/// read per [`WINDOW`]; a repair request for an early chunk simply loads
/// that window again.
pub struct Windowed<F> {
    /// The length the file had when it was measured. A read that comes back
    /// short of it means the file shrank since, and is an error.
    len: u64,
    /// The platform's ranged read.
    read: F,
    /// The last window read: where it starts and its bytes. Behind a mutex
    /// because the transfer reads chunks from more than one task (the first
    /// pass and repairs).
    window: Mutex<Option<(u64, Vec<u8>)>>,
}

impl<F: Fn(u64, usize) -> io::Result<Vec<u8>>> Windowed<F> {
    /// A file of `len` bytes, read through `read`.
    pub fn new(len: u64, read: F) -> Self {
        Self { len, read, window: Mutex::new(None) }
    }

    /// The file's length as measured.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the file has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Exactly `len` bytes from `offset`, from the window kept if it covers
    /// them and from a fresh window starting at `offset` if not.
    ///
    /// Asking past the end is an error, not a panic: the index behind a
    /// chunk read can come from the peer's repair request. A window that
    /// comes back short means the file shrank after it was measured, which
    /// is reported rather than papered over.
    pub fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let end = offset.checked_add(len as u64).filter(|end| *end <= self.len).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("{len} bytes at {offset} is past the end of the file"))
        })?;
        let mut window = self.window.lock().unwrap();
        if let Some(bytes) = covered(window.as_ref(), offset, end) {
            return Ok(bytes.to_vec());
        }
        let want = (self.len - offset).min(WINDOW.max(len) as u64) as usize;
        let bytes = (self.read)(offset, want)?;
        if bytes.len() < want {
            return Err(io::Error::other("the file changed after it was offered: it got shorter"));
        }
        let out = bytes[..len].to_vec();
        *window = Some((offset, bytes));
        Ok(out)
    }

    /// The whole file as a stream from the start, for hashing it without
    /// holding it.
    pub fn reader(&self) -> Sequential<'_, F> {
        Sequential { file: self, at: 0 }
    }
}

/// The bytes `start..end` of the kept window, if it holds all of them.
fn covered(window: Option<&(u64, Vec<u8>)>, start: u64, end: u64) -> Option<&[u8]> {
    let (at, bytes) = window?;
    let window_end = at + bytes.len() as u64;
    if start < *at || end > window_end {
        return None;
    }
    Some(&bytes[(start - at) as usize..(end - at) as usize])
}

/// A [`Windowed`] file read front to back, as [`std::io::Read`].
pub struct Sequential<'a, F> {
    /// The file being read.
    file: &'a Windowed<F>,
    /// How far the reading has got.
    at: u64,
}

impl<F: Fn(u64, usize) -> io::Result<Vec<u8>>> Read for Sequential<'_, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.file.len - self.at;
        let n = (buf.len() as u64).min(left).min(WINDOW as u64) as usize;
        if n == 0 {
            return Ok(0);
        }
        let bytes = self.file.read_at(self.at, n)?;
        buf[..n].copy_from_slice(&bytes);
        self.at += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A file of `len` bytes, each its offset mod 251, and a counter of
    /// how many times the "platform" was asked.
    fn file(len: usize) -> (Vec<u8>, Cell<usize>) {
        ((0..len).map(|i| (i % 251) as u8).collect(), Cell::new(0))
    }

    #[test]
    fn small_reads_come_from_one_window() {
        let (bytes, trips) = file(WINDOW + 100);
        let windowed = Windowed::new(bytes.len() as u64, |offset, len| {
            trips.set(trips.get() + 1);
            let start = offset as usize;
            Ok(bytes[start..(start + len).min(bytes.len())].to_vec())
        });
        for i in 0..100u64 {
            assert_eq!(windowed.read_at(i * 3800, 3800).unwrap(), bytes[i as usize * 3800..i as usize * 3800 + 3800]);
        }
        assert_eq!(trips.get(), 1, "a hundred chunks inside one window are one trip");
        // Running past the window loads the next one.
        let tail = windowed.read_at(WINDOW as u64, 100).unwrap();
        assert_eq!(tail, bytes[WINDOW..]);
        assert_eq!(trips.get(), 2);
        // A repair for an early chunk goes back for it.
        assert_eq!(windowed.read_at(0, 10).unwrap(), bytes[..10]);
        assert_eq!(trips.get(), 3);
    }

    #[test]
    fn the_stream_reads_the_whole_file_in_order() {
        let (bytes, _) = file(WINDOW * 2 + 7);
        let windowed = Windowed::new(bytes.len() as u64, |offset, len| {
            let start = offset as usize;
            Ok(bytes[start..(start + len).min(bytes.len())].to_vec())
        });
        let mut out = Vec::new();
        windowed.reader().read_to_end(&mut out).unwrap();
        assert_eq!(out, bytes);
    }

    #[test]
    fn reading_past_the_end_is_an_error_not_a_panic() {
        let windowed = Windowed::new(10, |_, len| Ok(vec![0u8; len]));
        assert!(windowed.read_at(5, 6).is_err());
        assert!(windowed.read_at(u64::MAX, 1).is_err());
        assert_eq!(windowed.read_at(5, 5).unwrap().len(), 5);
    }

    #[test]
    fn a_file_that_shrank_is_reported() {
        // Measured at 100 bytes, now holds 40.
        let windowed = Windowed::new(100, |offset, len| Ok(vec![0u8; (40u64.saturating_sub(offset) as usize).min(len)]));
        let error = windowed.read_at(0, 10).unwrap_err();
        assert!(error.to_string().contains("changed"), "{error}");
    }

    #[test]
    fn an_empty_file_streams_nothing() {
        let windowed = Windowed::new(0, |_, _| -> io::Result<Vec<u8>> { panic!("nothing to read") });
        assert!(windowed.is_empty());
        let mut out = Vec::new();
        windowed.reader().read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }
}
