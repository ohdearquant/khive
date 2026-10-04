//! Size-capped reads of streams whose length the caller does not control.
//!
//! Each reader takes at most `max + 1` bytes from the stream: one byte past the
//! cap is enough to tell a stream of exactly `max` bytes from a longer one.
//! `None` means more than `max` bytes were available. How that is reported is
//! left to the caller, so each call site keeps its own error.

use std::io::{self, Read};

use tokio::io::{AsyncRead, AsyncReadExt};

/// The most bytes a read with cap `max` takes. Saturates so `u64::MAX` is a valid cap.
fn read_limit(max: u64) -> u64 {
    max.saturating_add(1)
}

fn within_bound(bytes: Vec<u8>, max: u64) -> Option<Vec<u8>> {
    if bytes.len() as u64 > max {
        return None;
    }
    Some(bytes)
}

/// Read `reader` to its end. `Some` holds every byte when the stream has at most
/// `max` bytes, `None` means it had more, and a read error is returned unchanged.
pub fn read_to_end_bounded(reader: impl Read, max: u64) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let mut limited = reader.take(read_limit(max));
    limited.read_to_end(&mut bytes)?;
    Ok(within_bound(bytes, max))
}

/// Async counterpart of [`read_to_end_bounded`] over a `tokio` reader.
pub async fn read_to_end_bounded_async(
    reader: impl AsyncRead + Unpin,
    max: u64,
) -> io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    let mut limited = reader.take(read_limit(max));
    limited.read_to_end(&mut bytes).await?;
    Ok(within_bound(bytes, max))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::ReadBuf;

    /// Yields `prefix`, then fails every later read.
    struct Flaky {
        prefix: &'static [u8],
    }

    impl Read for Flaky {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.prefix.is_empty() {
                return Err(io::Error::other("flaky reader failed"));
            }
            let len = self.prefix.len().min(buf.len());
            buf[..len].copy_from_slice(&self.prefix[..len]);
            self.prefix = &self.prefix[len..];
            Ok(len)
        }
    }

    impl AsyncRead for Flaky {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let this = self.get_mut();
            if this.prefix.is_empty() {
                return Poll::Ready(Err(io::Error::other("flaky reader failed")));
            }
            let len = this.prefix.len().min(buf.remaining());
            buf.put_slice(&this.prefix[..len]);
            this.prefix = &this.prefix[len..];
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn blocking_exact_max_bytes_are_returned_whole() {
        let data = [7_u8; 16];
        let bytes = read_to_end_bounded(&data[..], 16).unwrap();
        assert_eq!(bytes, Some(data.to_vec()));
    }

    #[test]
    fn blocking_one_byte_past_max_is_refused() {
        let data = [7_u8; 17];
        let bytes = read_to_end_bounded(&data[..], 16).unwrap();
        assert_eq!(bytes, None);
    }

    #[test]
    fn blocking_empty_reader_yields_an_empty_vector() {
        let bytes = read_to_end_bounded(io::empty(), 16).unwrap();
        assert_eq!(bytes, Some(Vec::new()));
    }

    #[test]
    fn blocking_maximum_cap_does_not_overflow() {
        let data = [7_u8; 4];
        let bytes = read_to_end_bounded(&data[..], u64::MAX).unwrap();
        assert_eq!(bytes, Some(data.to_vec()));
    }

    #[test]
    fn blocking_read_error_is_returned() {
        let reader = Flaky { prefix: b"abc" };
        let error = read_to_end_bounded(reader, 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "flaky reader failed");
    }

    #[test]
    fn blocking_stops_reading_one_byte_past_max() {
        let data = [7_u8; 100];
        let mut reader = &data[..];
        let bytes = read_to_end_bounded(&mut reader, 10).unwrap();
        assert_eq!(bytes, None);
        assert_eq!(reader.len(), 89);
    }

    #[tokio::test]
    async fn async_exact_max_bytes_are_returned_whole() {
        let data = [7_u8; 16];
        let bytes = read_to_end_bounded_async(&data[..], 16).await.unwrap();
        assert_eq!(bytes, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn async_one_byte_past_max_is_refused() {
        let data = [7_u8; 17];
        let bytes = read_to_end_bounded_async(&data[..], 16).await.unwrap();
        assert_eq!(bytes, None);
    }

    #[tokio::test]
    async fn async_empty_reader_yields_an_empty_vector() {
        let reader = tokio::io::empty();
        let bytes = read_to_end_bounded_async(reader, 16).await.unwrap();
        assert_eq!(bytes, Some(Vec::new()));
    }

    #[tokio::test]
    async fn async_maximum_cap_does_not_overflow() {
        let data = [7_u8; 4];
        let max = u64::MAX;
        let bytes = read_to_end_bounded_async(&data[..], max).await.unwrap();
        assert_eq!(bytes, Some(data.to_vec()));
    }

    #[tokio::test]
    async fn async_read_error_is_returned() {
        let flaky = Flaky { prefix: b"abc" };
        let error = read_to_end_bounded_async(flaky, 16).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "flaky reader failed");
    }

    #[tokio::test]
    async fn async_stops_reading_one_byte_past_max() {
        let data = [7_u8; 100];
        let mut rest = &data[..];
        let bytes = read_to_end_bounded_async(&mut rest, 10).await.unwrap();
        assert_eq!(bytes, None);
        assert_eq!(rest.len(), 89);
    }
}
