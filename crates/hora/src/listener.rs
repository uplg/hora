//! The listening socket, guarded: a bound on how many connections are served
//! at once, and a deadline on each one that stops making progress.
//!
//! Unbounded, the status page is a way to take the monitoring down: every
//! connection is a file descriptor of the process that also opens the
//! checks' sockets, and a client that connects and then says nothing was
//! kept for ever. A few hundred of those and the checks could no longer
//! open a socket.
//!
//! - **The bound.** At most [`Guarded::limit`] connections are open; the
//!   next one is left in the kernel's accept queue (where it costs this
//!   process nothing) until a slot frees. One client address may hold a
//!   share of them only ([`per_address_limit`]): the heartbeats of jobs and
//!   peers come in by this door too, and one machine must not be able to
//!   shut it.
//! - **The deadlines.** A connection is closed when it has sent nothing
//!   [`FRESH`] after being accepted, when a request it started has no
//!   answer under way [`REQUEST`] later (the handlers' own deadline is
//!   shorter: this one is for a request that never finishes arriving),
//!   when it has taken nothing of an answer for [`STALL`] - a client that
//!   stopped reading - and when nothing has moved for [`IDLE`] after an
//!   answer: an idle keep-alive connection.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

/// A connection that has sent nothing since it was accepted.
const FRESH: Duration = Duration::from_secs(30);
/// From the first byte of a request to the first byte of its answer. Above
/// the handlers' deadline (`hora_web`'s 30 s), which must answer first.
const REQUEST: Duration = Duration::from_secs(60);
const _: () = assert!(REQUEST.as_secs() > hora_web::REQUEST_TIMEOUT.as_secs());
/// An answer the client takes no byte of: each one holds its buffers, the
/// kernel's and ours, for as long as it is kept.
const STALL: Duration = Duration::from_secs(30);
/// After an answer, with nothing read or written since. Longer than the
/// idle timeout of the usual reverse proxies (60 to 120 s), so the proxy is
/// the one that closes a pooled connection, never Hora under its feet.
const IDLE: Duration = Duration::from_mins(3);

/// Connections served at once when `server.max_connections` is unset. Each
/// one that trickles a request in holds about 25 KiB until its deadline:
/// a full house of them is some 25 MiB, which a small container survives.
const DEFAULT_LIMIT: u32 = 1024;
/// Never fewer than this, whatever the open-files limit says.
const MIN_LIMIT: u32 = 16;
/// One log line a minute, at most, while the listener is full.
const FULL_LOG_EVERY: Duration = Duration::from_mins(1);

/// What one connection can cost at its worst - a client that asks for a
/// large answer and never reads it: the kernel's send buffer plus the
/// server's own, measured at about 95 KiB.
const WORST_CONNECTION_BYTES: u64 = 128 * 1024;

/// How many connections to serve at once.
///
/// - Configured: that number. Unset: [`DEFAULT_LIMIT`], or fewer when the
///   memory the process may use is small - a full house of the worst
///   connections ([`WORST_CONNECTION_BYTES`] each) may take a quarter of it,
///   which is 128 connections in a 64 MiB container.
/// - Either way, never more than half the open-files limit: the other half
///   belongs to the checks and the database.
pub(crate) fn connection_limit(
    configured: Option<u32>,
    open_files: Option<u64>,
    memory: Option<u64>,
) -> u32 {
    let narrow = |count: u64| u32::try_from(count).unwrap_or(u32::MAX);
    let wanted = configured.unwrap_or_else(|| {
        let affordable =
            memory.map_or(u32::MAX, |bytes| narrow(bytes / 4 / WORST_CONNECTION_BYTES));
        DEFAULT_LIMIT.min(affordable)
    });
    let room = open_files.map_or(u32::MAX, |files| narrow(files / 2));
    wanted.min(room).max(MIN_LIMIT)
}

/// Never fewer than this for one address, whatever the limit: a browser
/// opens six connections by itself.
const MIN_PER_ADDRESS: u32 = 16;

/// How many connections one client address may hold at once, `None` for no
/// limit.
///
/// - Configured: that number (0 = none).
/// - Unset: a quarter of `limit`, so that it takes several machines to fill
///   the listener - unless a reverse proxy stands in front
///   (`client_ip_header` is set): every connection then comes from the
///   proxy's address, which is no client's.
pub(crate) fn per_address_limit(
    configured: Option<u32>,
    limit: u32,
    behind_proxy: bool,
) -> Option<u32> {
    match configured {
        Some(0) => None,
        Some(count) => Some(count),
        None if behind_proxy => None,
        None => Some((limit / 4).max(MIN_PER_ADDRESS).min(limit)),
    }
}

/// The process's soft limit on open files, when it has one.
pub(crate) fn open_files_limit() -> Option<u64> {
    rustix::process::getrlimit(rustix::process::Resource::Nofile).current
}

/// The memory this process may use, where the platform says: the smaller of
/// its control group's limit (a container's `--memory`) and the machine's
/// own. `None` off Linux.
pub(crate) fn memory_limit() -> Option<u64> {
    let read = |path: &str| std::fs::read_to_string(path).ok();
    let group = read("/sys/fs/cgroup/memory.max")
        .or_else(|| read("/sys/fs/cgroup/memory/memory.limit_in_bytes"))
        .and_then(|text| cgroup_limit(&text));
    let machine = read("/proc/meminfo").and_then(|text| meminfo_total(&text));
    match (group, machine) {
        (Some(group), Some(machine)) => Some(group.min(machine)),
        (limit, None) | (None, limit) => limit,
    }
}

/// A control group's memory limit as its file spells it: bytes, or `max`
/// (version 2) for none.
fn cgroup_limit(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// `MemTotal` of `/proc/meminfo`, in bytes.
fn meminfo_total(text: &str) -> Option<u64> {
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("MemTotal:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    kib.checked_mul(1024)
}

/// The kernel's send buffer for one connection. Left to tune itself it
/// grows to megabytes for a client that asks for a large answer and does not
/// read it - memory the kernel charges to this process's container, a
/// megabyte a connection in the measurements. Fixed, it is what
/// [`WORST_CONNECTION_BYTES`] counts on (Linux reserves twice the number
/// asked for). The price is paid by the largest answers on a slow link only:
/// the pages, compressed, fit in one buffer.
const SEND_BUFFER_BYTES: u32 = 64 * 1024;

/// How many connections the kernel holds for us while every slot is taken.
const BACKLOG: u32 = 1024;

/// Bind `address` (`host:port`) like [`TcpListener::bind`], with the send
/// buffer of the connections it will accept fixed at [`SEND_BUFFER_BYTES`]
/// (accepted sockets inherit it from the listening one).
pub(crate) async fn bind(address: &str) -> io::Result<TcpListener> {
    let mut last = None;
    for candidate in tokio::net::lookup_host(address).await? {
        match bind_one(candidate) {
            Ok(listener) => return Ok(listener),
            Err(err) => last = Some(err),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "could not resolve to any address",
        )
    }))
}

fn bind_one(address: SocketAddr) -> io::Result<TcpListener> {
    let socket = if address.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    // As `TcpListener::bind` does: a restart must not wait out the sockets
    // of the previous run.
    #[cfg(unix)]
    socket.set_reuseaddr(true)?;
    // Best effort: a platform that refuses the size serves with its own.
    let _ = socket.set_send_buffer_size(SEND_BUFFER_BYTES);
    socket.bind(address)?;
    socket.listen(BACKLOG)
}

/// The connections open per client address (see [`hora_web::bucket`]).
type Held = Arc<Mutex<HashMap<IpAddr, u32>>>;

/// One connection counted against its address, until dropped.
struct AddressSlot {
    held: Held,
    address: IpAddr,
}

impl AddressSlot {
    /// Count one more connection for `address`, unless it already holds
    /// `limit`.
    fn take(held: &Held, address: IpAddr, limit: u32) -> Option<Self> {
        let mut counts = held.lock().unwrap_or_else(PoisonError::into_inner);
        let count = counts.entry(address).or_insert(0);
        if *count >= limit {
            return None;
        }
        *count += 1;
        Some(Self {
            held: Arc::clone(held),
            address,
        })
    }
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut counts = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&self.address) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.address);
            }
        }
    }
}

/// A [`TcpListener`] that serves at most `limit` connections at once, and at
/// most `per_address` of them for one client.
pub(crate) struct Guarded {
    inner: TcpListener,
    slots: Arc<Semaphore>,
    limit: u32,
    per_address: Option<u32>,
    held: Held,
    full_logged: Option<Instant>,
    refused_logged: Option<Instant>,
}

impl Guarded {
    pub(crate) fn new(inner: TcpListener, limit: u32, per_address: Option<u32>) -> Self {
        Self {
            inner,
            slots: Arc::new(Semaphore::new(limit as usize)),
            limit,
            per_address,
            held: Held::default(),
            full_logged: None,
            refused_logged: None,
        }
    }

    /// The count of `peer`'s address, one higher - or `Err` when it already
    /// holds its share. Loopback is this machine (a proxy beside Hora, the
    /// container's own health check): never limited.
    fn address_slot(&mut self, peer: SocketAddr) -> Result<Option<AddressSlot>, ()> {
        let Some(limit) = self.per_address else {
            return Ok(None);
        };
        // Asked of the address itself too: an IPv6 one is counted by its
        // /64, and the /64 of `::1` is `::`, which is no loopback.
        let address = hora_web::bucket(peer.ip());
        if peer.ip().is_loopback() || address.is_loopback() {
            return Ok(None);
        }
        if let Some(slot) = AddressSlot::take(&self.held, address, limit) {
            return Ok(Some(slot));
        }
        let now = Instant::now();
        if self
            .refused_logged
            .is_none_or(|at| now.duration_since(at) >= FULL_LOG_EVERY)
        {
            self.refused_logged = Some(now);
            tracing::warn!(
                client = %address,
                limit,
                "a client holds its share of the HTTP connections: its next ones are                  closed (server.max_connections_per_ip; behind a reverse proxy, set                  server.client_ip_header)"
            );
        }
        Err(())
    }

    async fn slot(&mut self) -> OwnedSemaphorePermit {
        if let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() {
            return slot;
        }
        let now = Instant::now();
        if self
            .full_logged
            .is_none_or(|at| now.duration_since(at) >= FULL_LOG_EVERY)
        {
            self.full_logged = Some(now);
            tracing::warn!(
                limit = self.limit,
                "every HTTP connection slot is taken: new connections wait \
                 (server.max_connections)"
            );
        }
        match Arc::clone(&self.slots).acquire_owned().await {
            Ok(slot) => slot,
            // The semaphore is never closed; were it, serve nothing more
            // rather than serve unbounded.
            Err(_closed) => std::future::pending().await,
        }
    }
}

impl Listener for Guarded {
    type Io = Connection<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // The slot first: a connection that would have none stays in the
        // kernel's queue, where it holds no descriptor of ours.
        let slot = self.slot().await;
        loop {
            let (stream, peer) = Listener::accept(&mut self.inner).await;
            // One address past its share: closed here, and the slot goes
            // to whoever is next in the queue.
            let Ok(address) = self.address_slot(peer) else {
                continue;
            };
            // Answers are written whole; waiting to coalesce them only
            // delays the last segment.
            let _ = stream.set_nodelay(true);
            let mut connection = Connection::new(stream, Some(slot));
            connection.address = address;
            return (connection, peer);
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// One accepted connection: its slot, held until it closes, and the deadline
/// of whatever it is doing now.
pub(crate) struct Connection<S> {
    stream: S,
    /// `None` only in the tests, which need no listener.
    _slot: Option<OwnedSemaphorePermit>,
    /// Its place in its address's count, when addresses are counted.
    address: Option<AddressSlot>,
    /// When the connection is given up on, unless it moves first.
    deadline: Instant,
    /// The timer behind `deadline`, re-armed only when a read or a write
    /// actually has to wait (so a busy connection never touches it).
    timer: Pin<Box<Sleep>>,
    /// A request has begun arriving and its answer has not begun leaving.
    receiving: bool,
    /// A write is waiting for the client to read.
    stalled: bool,
}

impl<S> Connection<S> {
    fn new(stream: S, slot: Option<OwnedSemaphorePermit>) -> Self {
        let deadline = Instant::now() + FRESH;
        Self {
            stream,
            _slot: slot,
            address: None,
            deadline,
            timer: Box::pin(tokio::time::sleep_until(deadline)),
            receiving: false,
            stalled: false,
        }
    }

    /// Bytes arrived: a request is on its way in, and has [`REQUEST`] to be
    /// answered. More bytes of the same request do not extend it.
    fn received(&mut self) {
        if !self.receiving {
            self.receiving = true;
            self.deadline = Instant::now() + REQUEST;
        }
    }

    /// Bytes left: the answer is under way, and each write buys [`IDLE`].
    fn sent(&mut self) {
        self.receiving = false;
        self.stalled = false;
        self.deadline = Instant::now() + IDLE;
    }

    /// A write has to wait for the client: it has [`STALL`] to take
    /// something, counted from when the wait began.
    fn blocked(&mut self) {
        if !self.stalled {
            self.stalled = true;
            self.deadline = Instant::now() + STALL;
        }
    }

    /// The I/O has to wait: until the deadline at most.
    fn wait<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        if self.timer.deadline() != self.deadline {
            let deadline = self.deadline;
            self.timer.as_mut().reset(deadline);
        }
        match self.timer.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "connection stalled",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Connection<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        match Pin::new(&mut self.stream).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() > before {
                    self.received();
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => self.wait(cx),
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Connection<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write(cx, buf) {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.sent();
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => {
                self.blocked();
                self.wait(cx)
            }
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match Pin::new(&mut self.stream).poll_write_vectored(cx, bufs) {
            Poll::Ready(Ok(written)) => {
                if written > 0 {
                    self.sent();
                }
                Poll::Ready(Ok(written))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => {
                self.blocked();
                self.wait(cx)
            }
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn the_limit_leaves_half_the_descriptors_to_the_checks() {
        assert_eq!(connection_limit(None, None, None), DEFAULT_LIMIT);
        assert_eq!(connection_limit(None, Some(1_048_576), None), DEFAULT_LIMIT);
        assert_eq!(connection_limit(None, Some(1024), None), 512);
        assert_eq!(connection_limit(Some(10_000), Some(1024), None), 512);
        assert_eq!(connection_limit(Some(100), Some(1024), None), 100);
        // A tiny limit still serves the page.
        assert_eq!(connection_limit(None, Some(20), None), MIN_LIMIT);
        assert_eq!(connection_limit(Some(0), None, None), MIN_LIMIT);
    }

    #[test]
    fn a_small_memory_limit_narrows_the_default_only() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(connection_limit(None, None, Some(64 * MIB)), 128);
        assert_eq!(connection_limit(None, None, Some(256 * MIB)), 512);
        assert_eq!(
            connection_limit(None, None, Some(4096 * MIB)),
            DEFAULT_LIMIT
        );
        assert_eq!(connection_limit(None, None, Some(MIB)), MIN_LIMIT);
        // What the operator asked for is theirs to size.
        assert_eq!(connection_limit(Some(2000), None, Some(64 * MIB)), 2000);
    }

    #[test]
    fn memory_limits_are_read_as_their_files_spell_them() {
        assert_eq!(cgroup_limit("67108864\n"), Some(67_108_864));
        assert_eq!(cgroup_limit("max\n"), None);
        let meminfo = "MemTotal:        2014968 kB\nMemFree:          123456 kB\n";
        assert_eq!(meminfo_total(meminfo), Some(2_014_968 * 1024));
        assert_eq!(meminfo_total("MemFree: 1 kB\n"), None);
    }

    #[test]
    fn this_process_has_an_open_files_limit() {
        assert!(open_files_limit().is_some_and(|limit| limit > 0));
    }

    /// `SO_SNDBUF` of an accepted connection, read through a socket of the
    /// same kind (tokio exposes the option on `TcpSocket` only).
    fn socket_send_buffer(stream: &TcpStream) -> u32 {
        let std = stream
            .as_fd()
            .try_clone_to_owned()
            .map(std::net::TcpStream::from)
            .expect("duplicate the descriptor");
        let socket = tokio::net::TcpSocket::from_std_stream(std);
        socket.send_buffer_size().expect("read SO_SNDBUF")
    }

    async fn pair(limit: u32) -> (Guarded, SocketAddr) {
        let inner = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = inner.local_addr().expect("address");
        (Guarded::new(inner, limit, None), address)
    }

    #[tokio::test]
    async fn the_listener_binds_names_and_addresses_and_bounds_its_send_buffer() {
        for address in ["127.0.0.1:0", "localhost:0"] {
            let listener = bind(address).await.expect(address);
            assert!(listener.local_addr().expect("address").port() > 0);
        }
        assert!(bind("127.0.0.1:not-a-port").await.is_err());

        let listener = bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        let mut guarded = Guarded::new(listener, 4, None);
        let _client = TcpStream::connect(address).await.expect("connect");
        let (connection, _) = guarded.accept().await;
        // What the accepted socket got is the listener's, not the megabytes
        // of auto-tuning (the kernel may round, or double, the figure).
        let size = socket_send_buffer(&connection.stream);
        assert!(
            (SEND_BUFFER_BYTES..=4 * SEND_BUFFER_BYTES).contains(&size),
            "send buffer of {size} bytes"
        );
    }

    #[test]
    fn one_address_gets_a_share_unless_a_proxy_fronts_the_listener() {
        assert_eq!(per_address_limit(None, 1024, false), Some(256));
        assert_eq!(per_address_limit(None, 128, false), Some(32));
        assert_eq!(per_address_limit(None, 32, false), Some(MIN_PER_ADDRESS));
        assert_eq!(per_address_limit(None, 8, false), Some(8));
        assert_eq!(per_address_limit(None, 1024, true), None);
        // The operator's word, proxy or not.
        assert_eq!(per_address_limit(Some(50), 1024, true), Some(50));
        assert_eq!(per_address_limit(Some(0), 1024, false), None);
    }

    #[test]
    fn an_address_is_counted_until_its_connections_close() {
        let held = Held::default();
        let client: IpAddr = "203.0.113.7".parse().expect("address");
        let other: IpAddr = "203.0.113.8".parse().expect("address");
        let first = AddressSlot::take(&held, client, 2).expect("first");
        let _second = AddressSlot::take(&held, client, 2).expect("second");
        assert!(AddressSlot::take(&held, client, 2).is_none(), "a third");
        // Another client is not its neighbour's keeper.
        assert!(AddressSlot::take(&held, other, 2).is_some());
        drop(first);
        assert!(AddressSlot::take(&held, client, 2).is_some());
        // Nothing is kept for an address with no connection left.
        assert_eq!(held.lock().expect("lock").len(), 1);
    }

    /// The listener's own bookkeeping, with addresses no test socket can
    /// come from: a client is held to its share, its /64 counts as one, and
    /// a closed connection gives its place back.
    #[tokio::test]
    async fn a_client_past_its_share_is_refused_until_one_of_its_connections_closes() {
        let inner = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let mut listener = Guarded::new(inner, 8, Some(2));
        let from = |address: &str| -> SocketAddr { address.parse().expect("address") };
        let first = listener
            .address_slot(from("203.0.113.7:4000"))
            .expect("first");
        assert!(first.is_some(), "a client's connection is counted");
        let _second = listener
            .address_slot(from("203.0.113.7:4001"))
            .expect("second");
        assert!(listener.address_slot(from("203.0.113.7:4002")).is_err());
        // Another client, and this machine, are not held to its count.
        assert!(listener.address_slot(from("203.0.113.8:4000")).is_ok());
        assert!(matches!(
            listener.address_slot(from("127.0.0.1:4000")),
            Ok(None)
        ));
        assert!(matches!(
            listener.address_slot(from("[::1]:4000")),
            Ok(None)
        ));
        drop(first);
        assert!(listener.address_slot(from("203.0.113.7:4003")).is_ok());

        // One IPv6 subscriber holds a whole /64: its addresses are one client.
        let _a = listener
            .address_slot(from("[2001:db8:1:2::1]:1"))
            .expect("a");
        let _b = listener
            .address_slot(from("[2001:db8:1:2:ffff::2]:1"))
            .expect("b");
        assert!(listener.address_slot(from("[2001:db8:1:2::3]:1")).is_err());
        assert!(listener.address_slot(from("[2001:db8:1:3::1]:1")).is_ok());

        // Without a share, nobody is counted.
        let inner = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let mut open = Guarded::new(inner, 8, None);
        for port in 0..20 {
            let peer = SocketAddr::from(([203, 0, 113, 7], 4000 + port));
            assert!(matches!(open.address_slot(peer), Ok(None)));
        }
    }

    /// hyper writes an answer's head and body in one vectored call: that
    /// path moves the deadline like a plain write, and stalls like one.
    #[tokio::test(start_paused = true)]
    async fn a_vectored_write_counts_as_progress_and_can_stall() {
        let (server, mut client) = tokio::io::duplex(8);
        let mut connection = Connection::new(server, None);
        client.write_all(b"ping").await.expect("write");
        let mut request = [0u8; 4];
        connection.read_exact(&mut request).await.expect("read");
        assert!(connection.receiving);

        let parts = [io::IoSlice::new(b"he"), io::IoSlice::new(b"ad")];
        let written = connection.write_vectored(&parts).await.expect("vectored");
        assert!(written > 0);
        assert!(
            !connection.receiving,
            "an answer under way ends the request"
        );
        assert_eq!(connection.deadline, Instant::now() + IDLE);
        connection.flush().await.expect("flush");

        // The pipe is 8 bytes and nobody reads: the next vectored write waits,
        // and gives up when the client has taken nothing for STALL.
        let filler = [io::IoSlice::new(b"1234")];
        let taken = connection
            .write_vectored(&filler)
            .await
            .expect("fills the pipe");
        assert_eq!(taken, 4);
        let started = Instant::now();
        let more = [io::IoSlice::new(b"more")];
        let error = connection
            .write_vectored(&more)
            .await
            .expect_err("a stalled write fails");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), STALL);
        connection.shutdown().await.expect("shutdown");
    }

    #[tokio::test]
    async fn loopback_is_never_counted() {
        let inner = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = inner.local_addr().expect("address");
        let mut listener = Guarded::new(inner, 8, Some(1));
        let _one = TcpStream::connect(address).await.expect("connect");
        let _two = TcpStream::connect(address).await.expect("connect");
        let _first = listener.accept().await;
        let second = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
        assert!(second.is_ok(), "this machine was held to a client's share");
    }

    #[tokio::test]
    async fn a_connection_past_the_limit_waits_for_a_slot() {
        let (mut listener, address) = pair(1).await;
        let _first_client = TcpStream::connect(address).await.expect("connect");
        let (first, _) = listener.accept().await;
        let _second_client = TcpStream::connect(address).await.expect("connect");

        // Connected (the kernel took it), but not accepted: no slot.
        let waiting = tokio::time::timeout(Duration::from_millis(200), listener.accept()).await;
        assert!(waiting.is_err(), "accepted past the limit");

        drop(first);
        let accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()).await;
        assert!(accepted.is_ok(), "the freed slot was not reused");
    }

    /// A connection over an in-memory pipe: with the clock paused, real
    /// sockets would race the timers.
    fn piped() -> (Connection<tokio::io::DuplexStream>, tokio::io::DuplexStream) {
        let (server, client) = tokio::io::duplex(1024);
        (Connection::new(server, None), client)
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_connection_is_closed() {
        let (mut connection, _client) = piped();
        let started = Instant::now();
        let mut buffer = [0u8; 16];
        let error = connection
            .read(&mut buffer)
            .await
            .expect_err("a stalled read fails");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), FRESH);
    }

    #[tokio::test(start_paused = true)]
    async fn a_trickled_request_does_not_extend_its_deadline() {
        let (mut connection, mut client) = piped();
        // One byte every 10 s: each read succeeds, the request never ends.
        let trickle = tokio::spawn(async move {
            loop {
                if client.write_all(b"a").await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        });
        let started = Instant::now();
        let mut buffer = [0u8; 16];
        let error = loop {
            match connection.read(&mut buffer).await {
                Ok(_) => {}
                Err(error) => break error,
            }
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // From the first byte, whatever arrived since.
        assert_eq!(started.elapsed(), REQUEST);
        trickle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn an_answered_connection_may_idle_longer() {
        let (mut connection, mut client) = piped();
        client.write_all(b"ping").await.expect("write");
        let mut buffer = [0u8; 4];
        connection.read_exact(&mut buffer).await.expect("read");
        connection.write_all(b"pong").await.expect("answer");

        // Past the request deadline, within the idle one: still open.
        let mut next = [0u8; 4];
        let quiet = tokio::time::timeout(REQUEST + Duration::from_secs(30), async {
            connection.read(&mut next).await
        })
        .await;
        assert!(quiet.is_err(), "closed while idle within its allowance");

        // The next request is read as usual, and has its own deadline.
        client.write_all(b"ping").await.expect("write");
        connection.read_exact(&mut next).await.expect("read");
        assert_eq!(&next, b"ping");
        let started = Instant::now();
        let error = connection
            .read(&mut next)
            .await
            .expect_err("an unanswered request stalls");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), REQUEST);
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_that_stops_reading_is_closed() {
        // A pipe of 8 bytes: the second write has nowhere to go.
        let (server, _client) = tokio::io::duplex(8);
        let mut connection = Connection::new(server, None);
        connection.write_all(b"12345678").await.expect("fits");
        let started = Instant::now();
        let error = connection
            .write_all(b"more")
            .await
            .expect_err("a stalled write fails");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), STALL);
    }
}
