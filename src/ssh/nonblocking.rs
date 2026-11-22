//! Helpers for driving an ssh2 session in non-blocking mode.
//!
//! After authentication every [`SshSession`](super::SshSession) is switched to
//! non-blocking mode so that one blocked channel can never hold the session's
//! internal lock (and with it every other channel on the same connection).
//! Operations then return `LIBSSH2_ERROR_EAGAIN` instead of waiting, and the
//! callers in this crate retry them with explicit deadlines.

use std::io;
use std::thread;
use std::time::{Duration, Instant};

/// Raw libssh2 error code for "operation would block".
const LIBSSH2_ERROR_EAGAIN: i32 = -37;

/// How long to sleep between retries of an operation that would block.
const RETRY_SLEEP: Duration = Duration::from_millis(2);

/// Whether an ssh2 error means "would block, try again".
pub fn is_eagain(err: &ssh2::Error) -> bool {
    matches!(err.code(), ssh2::ErrorCode::Session(LIBSSH2_ERROR_EAGAIN))
}

/// Whether an I/O error is transient for a non-blocking stream.
pub fn is_would_block(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// Retry an ssh2 operation while it reports EAGAIN, until `deadline`.
///
/// Returns the operation's result, or a timeout error naming `what` once the
/// deadline passes. libssh2 requires a call that returned EAGAIN to be
/// repeated with the same arguments, which is exactly what this does.
pub fn retry_eagain<T>(
    deadline: Instant,
    what: &str,
    mut op: impl FnMut() -> Result<T, ssh2::Error>,
) -> anyhow::Result<T> {
    loop {
        match op() {
            Ok(value) => return Ok(value),
            Err(e) if is_eagain(&e) => {
                if Instant::now() >= deadline {
                    anyhow::bail!("Timed out waiting for SSH to {}", what);
                }
                thread::sleep(RETRY_SLEEP);
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("SSH failed to {}", what))),
        }
    }
}

/// Read two streams (typically stdout and stderr of one channel) until `eof`
/// reports the end, without letting either stream starve the other.
///
/// Reading one stream to completion before the other can deadlock: if the
/// remote fills the channel window with data for the unread stream it stops
/// sending, while we wait forever for the other. This reads whatever is
/// available on both, and fails once `deadline` passes.
pub fn read_both_until_eof(
    first: &mut impl io::Read,
    second: &mut impl io::Read,
    eof: impl Fn() -> bool,
    deadline: Instant,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut out_first = Vec::new();
    let mut out_second = Vec::new();
    let mut buf = [0u8; 16 * 1024];

    loop {
        let mut progress = false;
        for (reader, out) in [
            (&mut *first as &mut dyn io::Read, &mut out_first),
            (&mut *second as &mut dyn io::Read, &mut out_second),
        ] {
            match reader.read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    progress = true;
                }
                Err(e) if is_would_block(&e) => {}
                Err(e) => return Err(e),
            }
        }

        if !progress {
            if eof() {
                return Ok((out_first, out_second));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for remote command output",
                ));
            }
            thread::sleep(RETRY_SLEEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;

    #[test]
    fn test_is_eagain() {
        let eagain = ssh2::Error::new(ssh2::ErrorCode::Session(-37), "would block");
        let other = ssh2::Error::new(ssh2::ErrorCode::Session(-7), "socket send");
        assert!(is_eagain(&eagain));
        assert!(!is_eagain(&other));
    }

    #[test]
    fn test_retry_eagain_retries_then_succeeds() {
        let calls = Cell::new(0);
        let result = retry_eagain(Instant::now() + Duration::from_secs(5), "test", || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(ssh2::Error::new(ssh2::ErrorCode::Session(-37), "again"))
            } else {
                Ok(42)
            }
        });
        assert_eq!(result.unwrap(), 42);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn test_retry_eagain_times_out() {
        let result: anyhow::Result<()> = retry_eagain(
            Instant::now() + Duration::from_millis(20),
            "open a channel",
            || Err(ssh2::Error::new(ssh2::ErrorCode::Session(-37), "again")),
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Timed out"), "{}", err);
        assert!(err.contains("open a channel"), "{}", err);
    }

    #[test]
    fn test_retry_eagain_propagates_other_errors() {
        let result: anyhow::Result<()> =
            retry_eagain(Instant::now() + Duration::from_secs(5), "exec", || {
                Err(ssh2::Error::new(ssh2::ErrorCode::Session(-7), "boom"))
            });
        assert!(result.is_err());
    }

    /// A reader that yields queued chunks, returning WouldBlock between them.
    struct ChunkReader {
        chunks: VecDeque<Option<Vec<u8>>>,
    }

    impl io::Read for ChunkReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.chunks.pop_front() {
                Some(Some(chunk)) => {
                    buf[..chunk.len()].copy_from_slice(&chunk);
                    Ok(chunk.len())
                }
                Some(None) => Err(io::ErrorKind::WouldBlock.into()),
                None => Ok(0),
            }
        }
    }

    #[test]
    fn test_read_both_interleaves_streams() {
        let mut stdout = ChunkReader {
            chunks: VecDeque::from(vec![None, None, Some(b"out".to_vec())]),
        };
        let mut stderr = ChunkReader {
            chunks: VecDeque::from(vec![Some(b"err1".to_vec()), Some(b"err2".to_vec())]),
        };
        let (out, err) = read_both_until_eof(
            &mut stdout,
            &mut stderr,
            || true,
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(out, b"out");
        assert_eq!(err, b"err1err2");
    }

    #[test]
    fn test_read_both_times_out_without_eof() {
        let mut a = ChunkReader {
            chunks: VecDeque::new(),
        };
        let mut b = ChunkReader {
            chunks: VecDeque::new(),
        };
        let err = read_both_until_eof(
            &mut a,
            &mut b,
            || false,
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn test_read_both_propagates_errors() {
        struct Broken;
        impl io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
        }
        let mut ok = ChunkReader {
            chunks: VecDeque::new(),
        };
        let res = read_both_until_eof(
            &mut ok,
            &mut Broken,
            || false,
            Instant::now() + Duration::from_secs(1),
        );
        assert!(res.is_err());
    }
}
