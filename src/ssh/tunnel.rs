//! Local Unix socket tunnel to a remote Unix socket (the Docker API) over SSH.
//!
//! bollard talks HTTP to a local Unix socket; every connection it opens is
//! bridged to the remote socket through its own `direct-streamlocal` channel.
//!
//! Design:
//!
//! * The ssh2 session serialises every channel operation behind one internal
//!   lock, so a blocking read on one channel would stall every other channel
//!   (and keepalives) on the same host. The session is therefore driven in
//!   non-blocking mode by a single pump thread that multiplexes all accepted
//!   connections, so an idle keep-alive connection can never block the next
//!   request.
//! * Each connection has bounded buffers in both directions and applies
//!   backpressure instead of blocking, so large requests or responses cannot
//!   deadlock.
//! * A connection ends when either side closes or fails. Errors close the
//!   local stream, which surfaces to bollard as a failed request instead of a
//!   silent hang.

use anyhow::{Context, Result};
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{debug, error, warn};

use super::nonblocking::{is_would_block, retry_eagain};

/// Upper bound on buffered bytes per direction per connection.
const MAX_BUFFER: usize = 256 * 1024;

/// How long to wait for the remote side to close after the local side has
/// finished sending (local EOF) before the connection is dropped. Docker
/// normally closes at once; this only reclaims connections it never closes,
/// while leaving room for output still streaming after a half-close.
const HALF_CLOSE_GRACE: Duration = Duration::from_secs(120);

/// How long opening a channel to the remote socket may take.
const CHANNEL_OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a single SSH operation may wait for the TCP send buffer to drain.
const OUTBOUND_STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to keep retrying a channel close before giving up on it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Keepalive interval used when the session reports none.
const DEFAULT_KEEPALIVE: Duration = Duration::from_secs(15);

/// Idle sleep bounds for the pump loop.
const MAX_IDLE_SLEEP_ACTIVE: Duration = Duration::from_millis(2);
const MAX_IDLE_SLEEP_IDLE: Duration = Duration::from_millis(10);

/// The remote half of a tunnelled connection.
///
/// All methods are non-blocking: they return `io::ErrorKind::WouldBlock` when
/// no progress is possible right now.
pub trait ChannelIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn write(&mut self, buf: &[u8]) -> io::Result<usize>;
    fn send_eof(&mut self) -> io::Result<()>;
    /// Whether the remote side has sent EOF and all its data has been read.
    fn remote_eof(&self) -> bool;
    fn close(&mut self) -> io::Result<()>;
}

/// The local half of a tunnelled connection.
pub trait LocalIo: Read + Write {
    fn shutdown_write(&self) -> io::Result<()>;
}

impl LocalIo for UnixStream {
    fn shutdown_write(&self) -> io::Result<()> {
        self.shutdown(std::net::Shutdown::Write)
    }
}

/// Outcome of one pump step on a connection.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StepOutcome {
    /// Bytes moved or state changed.
    pub progress: bool,
    /// The connection is finished and should be closed.
    pub done: bool,
}

/// One local connection bridged to one channel.
pub struct Conn<C: ChannelIo, L: LocalIo> {
    channel: C,
    local: L,
    to_remote: Vec<u8>,
    to_local: Vec<u8>,
    local_eof: bool,
    eof_sent: bool,
    remote_eof: bool,
    eof_sent_at: Option<Instant>,
}

impl<C: ChannelIo, L: LocalIo> Conn<C, L> {
    pub fn new(channel: C, local: L) -> Self {
        Self {
            channel,
            local,
            to_remote: Vec::new(),
            to_local: Vec::new(),
            local_eof: false,
            eof_sent: false,
            remote_eof: false,
            eof_sent_at: None,
        }
    }

    /// Move as much data as possible in both directions without blocking.
    ///
    /// An error means the connection failed and must be closed.
    pub fn step(&mut self, buf: &mut [u8], now: Instant) -> io::Result<StepOutcome> {
        let mut progress = false;

        // local -> buffer
        if !self.local_eof && self.to_remote.len() < MAX_BUFFER {
            let room = (MAX_BUFFER - self.to_remote.len()).min(buf.len());
            match self.local.read(&mut buf[..room]) {
                Ok(0) => {
                    self.local_eof = true;
                    progress = true;
                }
                Ok(n) => {
                    self.to_remote.extend_from_slice(&buf[..n]);
                    progress = true;
                }
                Err(e) if is_would_block(&e) => {}
                Err(e) => return Err(e),
            }
        }

        // buffer -> channel
        while !self.to_remote.is_empty() {
            match self.channel.write(&self.to_remote) {
                Ok(0) => break,
                Ok(n) => {
                    self.to_remote.drain(..n);
                    progress = true;
                }
                Err(e) if is_would_block(&e) => break,
                Err(e) => return Err(e),
            }
        }

        // Forward the local EOF once everything before it has been sent.
        if self.local_eof && self.to_remote.is_empty() && !self.eof_sent {
            match self.channel.send_eof() {
                Ok(()) => {
                    self.eof_sent = true;
                    self.eof_sent_at = Some(now);
                    progress = true;
                }
                Err(e) if is_would_block(&e) => {}
                Err(e) => return Err(e),
            }
        }

        // channel -> buffer
        if !self.remote_eof && self.to_local.len() < MAX_BUFFER {
            let room = (MAX_BUFFER - self.to_local.len()).min(buf.len());
            match self.channel.read(&mut buf[..room]) {
                Ok(0) => {
                    if self.channel.remote_eof() {
                        self.remote_eof = true;
                        progress = true;
                    }
                }
                Ok(n) => {
                    self.to_local.extend_from_slice(&buf[..n]);
                    progress = true;
                }
                Err(e) if is_would_block(&e) => {
                    if self.channel.remote_eof() {
                        self.remote_eof = true;
                        progress = true;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        // buffer -> local
        while !self.to_local.is_empty() {
            match self.local.write(&self.to_local) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "local connection accepted no data",
                    ));
                }
                Ok(n) => {
                    self.to_local.drain(..n);
                    progress = true;
                }
                Err(e) if is_would_block(&e) => break,
                Err(e) => return Err(e),
            }
        }

        // The remote closed and everything it sent has been delivered: tell
        // the local side and finish. Nothing sent after this could be served.
        if self.remote_eof && self.to_local.is_empty() {
            // The local peer may already be gone; that is fine at this point.
            let _ = self.local.shutdown_write();
            return Ok(StepOutcome {
                progress: true,
                done: true,
            });
        }

        // The local side finished but the remote never closed.
        if let Some(sent_at) = self.eof_sent_at
            && now.saturating_duration_since(sent_at) >= HALF_CLOSE_GRACE
        {
            return Ok(StepOutcome {
                progress,
                done: true,
            });
        }

        Ok(StepOutcome {
            progress,
            done: false,
        })
    }

    /// Give back the channel so it can be closed.
    pub fn into_channel(self) -> C {
        self.channel
    }
}

/// An ssh2 channel driven in non-blocking mode.
///
/// libssh2 requires that an operation which stopped half-way through sending
/// a packet (TCP send buffer full) is repeated before any other packet is
/// sent on the session; otherwise the next sender gets an error. So when an
/// operation would block on the outbound direction it is retried here until
/// the packet is out. Blocking on the inbound direction (e.g. channel window
/// exhausted, no data yet) returns WouldBlock immediately.
pub struct SshChannel {
    channel: ssh2::Channel,
    session: ssh2::Session,
}

impl SshChannel {
    pub fn new(channel: ssh2::Channel, session: ssh2::Session) -> Self {
        Self { channel, session }
    }

    fn drive<T>(
        &mut self,
        mut op: impl FnMut(&mut ssh2::Channel) -> io::Result<T>,
    ) -> io::Result<T> {
        let channel = &mut self.channel;
        drive_outbound(&self.session, || op(channel))
    }
}

/// Repeat `op` while it would block on sending (a packet is half-written to
/// the TCP socket), so no other packet is started on the session meanwhile.
/// Blocking on the inbound direction is returned to the caller as WouldBlock.
fn drive_outbound<T>(
    session: &ssh2::Session,
    mut op: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + OUTBOUND_STALL_TIMEOUT;
    loop {
        match op() {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && outbound_blocked(session) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "SSH connection stopped accepting data",
                    ));
                }
                thread::sleep(Duration::from_millis(1));
            }
            other => return other,
        }
    }
}

fn outbound_blocked(session: &ssh2::Session) -> bool {
    matches!(
        session.block_directions(),
        ssh2::BlockDirections::Outbound | ssh2::BlockDirections::Both
    )
}

impl ChannelIo for SshChannel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.drive(|ch| Read::read(ch, buf))
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Never call `flush` on an ssh2 channel: libssh2's channel flush
        // discards unread incoming data.
        self.drive(|ch| Write::write(ch, buf))
    }

    fn send_eof(&mut self) -> io::Result<()> {
        self.drive(|ch| ch.send_eof().map_err(io::Error::from))
    }

    fn remote_eof(&self) -> bool {
        self.channel.eof()
    }

    fn close(&mut self) -> io::Result<()> {
        self.drive(|ch| ch.close().map_err(io::Error::from))
    }
}

/// A private, per-process directory holding the local socket.
///
/// Created with mode 0700 under a unique name so other users and other korgi
/// processes can neither connect to nor replace our socket. Removed on drop.
#[derive(Debug)]
pub struct PrivateSocketDir {
    path: PathBuf,
}

impl PrivateSocketDir {
    pub fn create_in(parent: &Path) -> Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let mut last_err = None;
        for _ in 0..16 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("korgi-{}-{}-{:08x}", pid, n, nanos));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_err = Some(e),
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to create socket directory {}", path.display())
                    });
                }
            }
        }
        Err(last_err.unwrap_or_else(|| io::Error::other("no unique name found")))
            .with_context(|| format!("Failed to create socket directory in {}", parent.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PrivateSocketDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            debug!("Failed to remove {}: {}", self.path.display(), e);
        }
    }
}

/// A running tunnel. Stops the pump thread and removes the socket on drop.
pub struct Tunnel {
    socket_path: PathBuf,
    stop: Arc<AtomicBool>,
    // Field order matters: the directory is removed after the stop flag is set.
    _dir: PrivateSocketDir,
}

impl Tunnel {
    /// Start a tunnel from a fresh local socket to `remote_socket`.
    ///
    /// `session` must already be authenticated and in non-blocking mode;
    /// `open_lock` serialises channel opens with other users of the session
    /// (libssh2 keeps channel-open state per session).
    pub fn start(
        session: ssh2::Session,
        open_lock: Arc<Mutex<()>>,
        remote_socket: &str,
        host_name: &str,
    ) -> Result<Self> {
        let dir = PrivateSocketDir::create_in(&std::env::temp_dir())?;
        let socket_path = dir.path().join("docker.sock");
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind {}", socket_path.display()))?;
        listener
            .set_nonblocking(true)
            .context("Failed to make tunnel listener non-blocking")?;

        let stop = Arc::new(AtomicBool::new(false));
        let pump = Pump {
            listener,
            session,
            open_lock,
            remote_socket: remote_socket.to_string(),
            host_name: host_name.to_string(),
            stop: stop.clone(),
        };
        thread::Builder::new()
            .name(format!("korgi-tunnel-{}", host_name))
            .spawn(move || pump.run())
            .context("Failed to start tunnel thread")?;

        Ok(Self {
            socket_path,
            stop,
            _dir: dir,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Open a `direct-streamlocal` channel, retrying while the session would block.
pub fn open_streamlocal(
    session: &ssh2::Session,
    open_lock: &Mutex<()>,
    remote_socket: &str,
    timeout: Duration,
) -> Result<ssh2::Channel> {
    let _guard = open_lock.lock().unwrap_or_else(|p| p.into_inner());
    retry_eagain(
        Instant::now() + timeout,
        &format!("open a channel to {}", remote_socket),
        || session.channel_direct_streamlocal(remote_socket, None),
    )
}

struct Pump {
    listener: UnixListener,
    session: ssh2::Session,
    open_lock: Arc<Mutex<()>>,
    remote_socket: String,
    host_name: String,
    stop: Arc<AtomicBool>,
}

impl Pump {
    fn run(self) {
        let mut conns: Vec<Conn<SshChannel, UnixStream>> = Vec::new();
        let mut closing: Vec<(SshChannel, Instant)> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut next_keepalive = Instant::now() + DEFAULT_KEEPALIVE;
        let mut idle_sleep = Duration::from_micros(100);

        while !self.stop.load(Ordering::Relaxed) {
            let mut progress = false;

            // Accept new local connections.
            loop {
                match self.listener.accept() {
                    Ok((stream, _)) => {
                        progress = true;
                        if let Some(conn) = self.open_conn(stream) {
                            conns.push(conn);
                        }
                    }
                    Err(e) if is_would_block(&e) => break,
                    Err(e) => {
                        error!("Tunnel listener failed on {}: {}", self.host_name, e);
                        return;
                    }
                }
            }

            // Pump every connection.
            let now = Instant::now();
            let mut i = 0;
            while i < conns.len() {
                match conns[i].step(&mut buf, now) {
                    Ok(outcome) => {
                        progress |= outcome.progress;
                        if outcome.done {
                            closing.push((conns.swap_remove(i).into_channel(), now));
                            continue;
                        }
                    }
                    Err(e) => {
                        debug!("Tunnel connection on {} failed: {}", self.host_name, e);
                        progress = true;
                        // Dropping the local stream makes bollard see the failure.
                        closing.push((conns.swap_remove(i).into_channel(), now));
                        continue;
                    }
                }
                i += 1;
            }

            // Finish closing channels.
            closing.retain_mut(|(channel, since)| match channel.close() {
                Ok(()) => false,
                Err(e) if is_would_block(&e) => {
                    now.saturating_duration_since(*since) < CLOSE_TIMEOUT
                }
                Err(e) => {
                    debug!("Closing tunnel channel on {} failed: {}", self.host_name, e);
                    false
                }
            });

            // Keep the SSH connection alive through NAT / firewalls.
            if now >= next_keepalive {
                next_keepalive = now
                    + match drive_outbound(&self.session, || {
                        self.session.keepalive_send().map_err(io::Error::from)
                    }) {
                        Ok(0) => DEFAULT_KEEPALIVE,
                        Ok(secs) => Duration::from_secs(u64::from(secs)),
                        Err(e) if is_would_block(&e) => Duration::from_secs(1),
                        Err(e) => {
                            warn!("SSH keepalive failed on {}: {}", self.host_name, e);
                            DEFAULT_KEEPALIVE
                        }
                    };
            }

            if progress {
                idle_sleep = Duration::from_micros(100);
            } else {
                thread::sleep(idle_sleep);
                let max = if conns.is_empty() && closing.is_empty() {
                    MAX_IDLE_SLEEP_IDLE
                } else {
                    MAX_IDLE_SLEEP_ACTIVE
                };
                idle_sleep = (idle_sleep * 2).min(max);
            }
        }
        debug!("Tunnel to {} stopped", self.host_name);
    }

    fn open_conn(&self, stream: UnixStream) -> Option<Conn<SshChannel, UnixStream>> {
        if let Err(e) = stream.set_nonblocking(true) {
            error!(
                "Tunnel on {}: failed to configure connection: {}",
                self.host_name, e
            );
            return None;
        }
        match open_streamlocal(
            &self.session,
            &self.open_lock,
            &self.remote_socket,
            CHANNEL_OPEN_TIMEOUT,
        ) {
            Ok(channel) => Some(Conn::new(
                SshChannel::new(channel, self.session.clone()),
                stream,
            )),
            Err(e) => {
                // Dropping the stream fails the pending request immediately.
                error!("Tunnel on {}: {:#}", self.host_name, e);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    /// Shared state of a fake channel, inspectable from the test.
    #[derive(Default)]
    struct FakeState {
        /// Bytes the "remote" will deliver, in chunks; `None` = WouldBlock.
        incoming: VecDeque<Option<Vec<u8>>>,
        /// Remote sends EOF once `incoming` is empty.
        remote_closes: bool,
        /// Everything written to the remote.
        written: Vec<u8>,
        /// Maximum bytes accepted per write (simulates a small window).
        write_chunk: usize,
        /// Writes that return WouldBlock before accepting data.
        write_stalls: usize,
        eof_received: bool,
        closed: bool,
        fail_writes: bool,
    }

    #[derive(Clone)]
    struct FakeChannel(Arc<StdMutex<FakeState>>);

    impl FakeChannel {
        fn new() -> Self {
            Self(Arc::new(StdMutex::new(FakeState {
                write_chunk: usize::MAX,
                ..Default::default()
            })))
        }
        fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
            self.0.lock().unwrap()
        }
    }

    impl ChannelIo for FakeChannel {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut s = self.state();
            match s.incoming.pop_front() {
                Some(Some(mut chunk)) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    if n < chunk.len() {
                        chunk.drain(..n);
                        s.incoming.push_front(Some(chunk));
                    }
                    Ok(n)
                }
                Some(None) => Err(io::ErrorKind::WouldBlock.into()),
                None if s.remote_closes => Ok(0),
                None => Err(io::ErrorKind::WouldBlock.into()),
            }
        }
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut s = self.state();
            if s.fail_writes {
                return Err(io::Error::other("channel broken"));
            }
            if s.write_stalls > 0 {
                s.write_stalls -= 1;
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = buf.len().min(s.write_chunk);
            s.written.extend_from_slice(&buf[..n]);
            // Stall again after each accepted chunk to exercise buffering.
            s.write_stalls = 1;
            Ok(n)
        }
        fn send_eof(&mut self) -> io::Result<()> {
            self.state().eof_received = true;
            Ok(())
        }
        fn remote_eof(&self) -> bool {
            let s = self.state();
            s.remote_closes && s.incoming.is_empty()
        }
        fn close(&mut self) -> io::Result<()> {
            self.state().closed = true;
            Ok(())
        }
    }

    fn local_pair() -> (UnixStream, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        (a, b)
    }

    /// Step `conns` until `pred` holds or the iteration budget runs out.
    fn run_until(
        conns: &mut [Conn<FakeChannel, UnixStream>],
        now: Instant,
        mut pred: impl FnMut(&[Conn<FakeChannel, UnixStream>]) -> bool,
    ) -> Vec<io::Result<StepOutcome>> {
        let mut buf = vec![0u8; 8192];
        let mut last = Vec::new();
        for _ in 0..10_000 {
            last = conns.iter_mut().map(|c| c.step(&mut buf, now)).collect();
            if pred(conns) {
                break;
            }
        }
        last
    }

    #[test]
    fn test_round_trip_with_idle_keepalive_connection() {
        let channel = FakeChannel::new();
        channel
            .state()
            .incoming
            .push_back(Some(b"HTTP/1.1 200 OK\r\n\r\nok".to_vec()));
        let (pump_side, mut client) = local_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut conns = [Conn::new(channel.clone(), pump_side)];

        client.write_all(b"GET /_ping HTTP/1.1\r\n\r\n").unwrap();
        run_until(&mut conns, Instant::now(), |_| {
            channel.state().written.len() >= 23
        });
        assert_eq!(channel.state().written, b"GET /_ping HTTP/1.1\r\n\r\n");

        let mut resp = [0u8; 21];
        client.read_exact(&mut resp).unwrap();
        assert_eq!(&resp, b"HTTP/1.1 200 OK\r\n\r\nok");

        // The client keeps the connection open: stepping must not block and
        // must not finish the connection.
        let mut buf = vec![0u8; 1024];
        for _ in 0..100 {
            let out = conns[0].step(&mut buf, Instant::now()).unwrap();
            assert!(!out.done);
        }
    }

    #[test]
    fn test_idle_connection_does_not_block_another() {
        let idle_channel = FakeChannel::new();
        let (idle_side, _idle_client) = local_pair();

        let busy_channel = FakeChannel::new();
        busy_channel
            .state()
            .incoming
            .push_back(Some(b"pong".to_vec()));
        let (busy_side, mut busy_client) = local_pair();
        busy_client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut conns = [
            Conn::new(idle_channel, idle_side),
            Conn::new(busy_channel.clone(), busy_side),
        ];
        busy_client.write_all(b"ping").unwrap();
        run_until(&mut conns, Instant::now(), |_| {
            busy_channel.state().written == b"ping"
        });
        let mut resp = [0u8; 4];
        busy_client.read_exact(&mut resp).unwrap();
        assert_eq!(&resp, b"pong");
    }

    #[test]
    fn test_large_request_and_response_do_not_deadlock() {
        let channel = FakeChannel::new();
        channel.state().write_chunk = 1000;
        let response: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        for chunk in response.chunks(7000) {
            channel.state().incoming.push_back(Some(chunk.to_vec()));
            channel.state().incoming.push_back(None);
        }

        let (pump_side, client) = local_pair();
        let request: Vec<u8> = (0..200_000u32).map(|i| (i % 241) as u8).collect();

        // Client writes the whole request and reads the whole response on its
        // own thread, like hyper would.
        let req = request.clone();
        let handle = thread::spawn(move || {
            let mut client = client;
            client
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut w = client.try_clone().unwrap();
            let writer = thread::spawn(move || w.write_all(&req).unwrap());
            let mut got = Vec::new();
            client.read_to_end(&mut got).unwrap();
            writer.join().unwrap();
            got
        });

        let mut conns = [Conn::new(channel.clone(), pump_side)];
        let mut buf = vec![0u8; 8192];
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                // Like an HTTP server, close only after the whole request arrived.
                let mut s = channel.state();
                if s.written.len() == request.len() {
                    s.remote_closes = true;
                }
            }
            let out = conns[0].step(&mut buf, Instant::now()).unwrap();
            if out.done {
                break;
            }
            assert!(Instant::now() < deadline, "pump did not finish");
            if !out.progress {
                thread::sleep(Duration::from_micros(200));
            }
        }
        drop(conns);
        let got = handle.join().unwrap();
        assert_eq!(got.len(), response.len());
        assert_eq!(got, response);
        assert_eq!(channel.state().written, request);
    }

    #[test]
    fn test_remote_eof_finishes_connection_and_signals_local() {
        let channel = FakeChannel::new();
        {
            let mut s = channel.state();
            s.incoming.push_back(Some(b"bye".to_vec()));
            s.remote_closes = true;
        }
        let (pump_side, mut client) = local_pair();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut conns = [Conn::new(channel, pump_side)];
        let results = run_until(&mut conns, Instant::now(), |_| false);
        assert!(results[0].as_ref().unwrap().done);

        let mut got = Vec::new();
        client.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"bye");
    }

    #[test]
    fn test_local_close_sends_eof_then_finishes_on_remote_eof() {
        let channel = FakeChannel::new();
        let (pump_side, client) = local_pair();
        drop(client);
        let mut conns = [Conn::new(channel.clone(), pump_side)];
        let now = Instant::now();
        let mut buf = vec![0u8; 1024];
        let out = conns[0].step(&mut buf, now).unwrap();
        assert!(!out.done);
        assert!(channel.state().eof_received);

        channel.state().remote_closes = true;
        let out = conns[0].step(&mut buf, now).unwrap();
        assert!(out.done);
    }

    #[test]
    fn test_half_closed_connection_times_out() {
        let channel = FakeChannel::new();
        let (pump_side, client) = local_pair();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut conns = [Conn::new(channel.clone(), pump_side)];
        let start = Instant::now();
        let mut buf = vec![0u8; 1024];
        assert!(!conns[0].step(&mut buf, start).unwrap().done);
        assert!(channel.state().eof_received);
        assert!(
            !conns[0]
                .step(&mut buf, start + Duration::from_secs(1))
                .unwrap()
                .done
        );
        assert!(
            conns[0]
                .step(&mut buf, start + HALF_CLOSE_GRACE)
                .unwrap()
                .done
        );
    }

    #[test]
    fn test_channel_write_error_propagates() {
        let channel = FakeChannel::new();
        channel.state().fail_writes = true;
        let (pump_side, mut client) = local_pair();
        client.write_all(b"POST /x").unwrap();
        let mut conns = [Conn::new(channel, pump_side)];
        let mut buf = vec![0u8; 1024];
        assert!(conns[0].step(&mut buf, Instant::now()).is_err());
    }

    #[test]
    fn test_local_write_error_propagates() {
        let channel = FakeChannel::new();
        channel.state().incoming.push_back(Some(vec![1u8; 4096]));
        let (pump_side, client) = local_pair();
        drop(client);
        let mut conns = [Conn::new(channel, pump_side)];
        let mut buf = vec![0u8; 8192];
        let mut saw_err = false;
        for _ in 0..10 {
            match conns[0].step(&mut buf, Instant::now()) {
                Err(_) => {
                    saw_err = true;
                    break;
                }
                Ok(o) if o.done => panic!("finished without reporting the write error"),
                Ok(_) => {}
            }
        }
        assert!(saw_err);
    }

    #[test]
    fn test_private_socket_dir_is_0700_unique_and_removed() {
        use std::os::unix::fs::PermissionsExt;
        let parent = tempfile::TempDir::new().unwrap();
        let a = PrivateSocketDir::create_in(parent.path()).unwrap();
        let b = PrivateSocketDir::create_in(parent.path()).unwrap();
        assert_ne!(a.path(), b.path());
        let mode = std::fs::metadata(a.path()).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        let path = a.path().to_path_buf();
        std::fs::write(path.join("docker.sock"), b"").unwrap();
        drop(a);
        assert!(!path.exists());
    }

    #[test]
    fn test_private_socket_dir_reports_create_errors() {
        let err = PrivateSocketDir::create_in(Path::new("/nonexistent/korgi/parent")).unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to create socket directory")
        );
    }
}
