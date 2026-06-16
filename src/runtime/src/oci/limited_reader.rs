//! A `Read` adapter that fails once a byte budget is exceeded.
//!
//! Layer/archive extraction streams a decompressor (gzip/zstd/bzip2/xz) into a
//! tar reader with no bound on the *decompressed* output. A compression-bomb
//! layer (a few MB that expands to hundreds of GB) would therefore fill the
//! host disk during `pull`/build and DoS the host. Wrapping the decompressor in
//! [`LimitedReader`] caps total decompressed bytes and aborts with a clear error
//! instead — mirroring the existing `MAX_ADD_URL_BYTES` cap on the ADD-URL path.

use std::io::{self, Read};

/// Wraps a reader and returns an error once more than `limit` bytes have been
/// read in total. Use it around a decompressor so the tar layer above it cannot
/// pull an unbounded amount of decompressed data to disk.
pub(crate) struct LimitedReader<R> {
    inner: R,
    remaining: u64,
    limit: u64,
}

impl<R: Read> LimitedReader<R> {
    pub(crate) fn new(inner: R, limit: u64) -> Self {
        Self {
            inner,
            remaining: limit,
            limit,
        }
    }
}

impl<R: Read> Read for LimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n as u64 > self.remaining {
            return Err(io::Error::other(format!(
                "decompressed output exceeded the {}-byte limit (possible decompression bomb); \
                 raise the limit via the relevant A3S_BOX_MAX_*_BYTES env var if this is a \
                 legitimately large image/archive",
                self.limit
            )));
        }
        self.remaining -= n as u64;
        Ok(n)
    }
}

/// Read a byte-size cap from `var`, falling back to `default_bytes` when unset or
/// unparseable. Lets operators tune the decompression-bomb ceilings.
pub(crate) fn cap_from_env(var: &str, default_bytes: u64) -> u64 {
    std::env::var(var)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(default_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_once_total_read_exceeds_limit() {
        // 10 KiB of data, 4 KiB cap → must error before delivering it all.
        let data = vec![0u8; 10 * 1024];
        let mut r = LimitedReader::new(&data[..], 4 * 1024);
        let mut sink = Vec::new();
        let err = std::io::copy(&mut r, &mut sink).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(sink.len() as u64 <= 4 * 1024, "must not exceed the cap");
        assert!(err.to_string().contains("decompression bomb"));
    }

    #[test]
    fn passes_data_under_the_limit() {
        let data = vec![7u8; 4096];
        let mut r = LimitedReader::new(&data[..], 1024 * 1024);
        let mut sink = Vec::new();
        std::io::copy(&mut r, &mut sink).unwrap();
        assert_eq!(sink, data);
    }
}
