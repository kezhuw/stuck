use std::io;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::time::Duration;

use ignore_result::Ignore;
use mio::net;
use static_assertions::{assert_impl_all, assert_not_impl_any};

use crate::runtime::Scheduler;
use crate::task::mpsc;

/// Listener for incoming TCP connections.
pub struct TcpListener {
    listener: net::TcpListener,
    readable: mpsc::Receiver<()>,
}

assert_impl_all!(TcpListener: Send, Sync);

impl TcpListener {
    /// Binds and listens to given socket address.
    pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
        let mut listener = net::TcpListener::bind(addr)?;
        let registry = unsafe { Scheduler::registry() };
        let readable = registry.register_tcp_listener(&mut listener)?;
        Ok(TcpListener { listener, readable })
    }

    /// Accepts an incoming connection.
    pub fn accept(&mut self) -> io::Result<(TcpStream, SocketAddr)> {
        loop {
            match self.listener.accept() {
                Ok((stream, addr)) => {
                    let stream = TcpStream::new(stream)?;
                    return Ok((stream, addr));
                },
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    self.readable.recv().expect("runtime closing");
                },
                Err(err) => return Err(err),
            }
        }
    }

    /// Returns the local socket address this listener bound to.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Sets the time-to-live (aka. TTL or hop limit) option for ip packets sent from incoming connections.
    ///
    /// Accepted connections will inherit this option.
    pub fn set_ttl(&self, ttl: u8) -> io::Result<()> {
        self.listener.set_ttl(ttl.into())
    }

    /// Gets the time-to-live option for this listening socket.
    pub fn ttl(&self) -> io::Result<u8> {
        self.listener.ttl().map(|ttl| ttl as u8)
    }
}

/// A TCP stream between a local and a remote socket.
pub struct TcpStream {
    stream: net::TcpStream,
    readable: mpsc::Receiver<()>,
    writable: mpsc::Receiver<()>,
}

assert_impl_all!(TcpStream: Send, Sync);

impl TcpStream {
    fn new(mut stream: net::TcpStream) -> io::Result<Self> {
        let registry = unsafe { Scheduler::registry() };
        let (readable, mut writable) = registry.register_tcp_stream(&mut stream)?;
        writable.recv().expect("runtime closing");
        Ok(TcpStream { stream, readable, writable })
    }

    /// Sets the time-to-live (aka. TTL or hop limit) option for ip packets sent from this socket.
    pub fn set_ttl(&self, ttl: u8) -> io::Result<()> {
        self.stream.set_ttl(ttl.into())
    }

    /// Gets the time-to-live option for this socket.
    pub fn ttl(&self) -> io::Result<u8> {
        self.stream.ttl().map(|ttl| ttl as u8)
    }

    /// Connects to remote host.
    pub fn connect(addr: SocketAddr) -> io::Result<Self> {
        let mut stream = net::TcpStream::connect(addr)?;
        let registry = unsafe { Scheduler::registry() };
        let (readable, mut writable) = registry.register_tcp_stream(&mut stream)?;
        writable.recv().expect("runtime closing");
        if let Some(err) = stream.take_error()? {
            return Err(err);
        }
        Ok(TcpStream { stream, readable, writable })
    }

    /// Sets the value of the `TCP_NODELAY` option on this socket.
    ///
    /// If set, this option disables the Nagle algorithm. This means that segments are always
    /// sent as soon as possible, even if there is only a small amount of data. When not set,
    /// data is buffered until there is a sufficient amount to send out, thereby avoiding the
    /// frequent sending of small packets.
    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        self.stream.set_nodelay(nodelay)
    }

    /// Gets the value of the `TCP_NODELAY` option on this socket.
    ///
    /// For more information about this option, see [TcpStream::set_nodelay].
    pub fn nodelay(&self) -> io::Result<bool> {
        self.stream.nodelay()
    }

    /// Sets the value of the `SO_LINGER` option on this socket.
    ///
    /// This value controls how the socket is closed when data remains to be sent. If `SO_LINGER`
    /// is set, the socket will remain open for the specified duration as the system attempts to
    /// send pending data. Otherwise, the system may close the socket immediately, or wait for a
    /// default timeout.
    pub fn set_linger(&self, linger: Option<Duration>) -> io::Result<()> {
        let fd = self.stream.as_raw_fd();
        let linger = libc::linger {
            l_onoff: if linger.is_some() { 1 } else { 0 },
            l_linger: linger.map(|d| d.as_secs() as libc::c_int).unwrap_or_default(),
        };
        let rc = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &linger as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::linger>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Gets the value of the `SO_LINGER` option on this socket.
    ///
    /// For more information about this option, see [TcpStream::set_linger].
    pub fn linger(&self) -> io::Result<Option<Duration>> {
        let fd = self.stream.as_raw_fd();
        let mut linger: libc::linger = unsafe { std::mem::zeroed() };
        let mut optlen = std::mem::size_of::<libc::linger>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_LINGER,
                &mut linger as *mut _ as *mut libc::c_void,
                &mut optlen,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((linger.l_onoff != 0).then(|| Duration::from_secs(linger.l_linger as u64)))
    }

    /// Returns the local socket address of this connection.
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.stream.local_addr()
    }

    /// Returns the remote socket address of this connection.
    pub fn peer_addr(&self) -> io::Result<SocketAddr> {
        self.stream.peer_addr()
    }

    /// Splits this connection to reader and writer.
    pub fn into_split(self) -> (TcpReader, TcpWriter) {
        let stream = Rc::new(self.stream);
        let reader = TcpReader { stream: stream.clone(), readable: self.readable };
        let writer = TcpWriter { stream, writable: self.writable };
        (reader, writer)
    }

    /// Shuts down read half of this connection.
    pub fn shutdown_read(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Read)
    }

    /// Shuts down write half of this connection.
    pub fn shutdown_write(&self) -> io::Result<()> {
        self.stream.shutdown(std::net::Shutdown::Write)
    }

    fn read(stream: &mut net::TcpStream, readable: &mut mpsc::Receiver<()>, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match stream.read(buf) {
                Ok(n) => return Ok(n),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    readable.recv().expect("runtime closing");
                },
                Err(err) => return Err(err),
            }
        }
    }

    fn write(stream: &mut net::TcpStream, writable: &mut mpsc::Receiver<()>, buf: &[u8]) -> io::Result<usize> {
        loop {
            match stream.write(buf) {
                Ok(n) => return Ok(n),
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    writable.recv().expect("runtime closing");
                },
                Err(err) => return Err(err),
            }
        }
    }
}

impl AsRawFd for TcpStream {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

/// Read half of [TcpStream].
///
/// The read half of this connection will be shutdown when this value is dropped.
pub struct TcpReader {
    stream: Rc<net::TcpStream>,
    readable: mpsc::Receiver<()>,
}

assert_not_impl_any!(TcpReader: Send, Sync);

impl Drop for TcpReader {
    fn drop(&mut self) {
        self.stream.shutdown(std::net::Shutdown::Read).ignore();
    }
}

impl io::Read for TcpReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let stream = Rc::as_ptr(&self.stream) as *mut _;
        TcpStream::read(unsafe { &mut *stream }, &mut self.readable, buf)
    }
}

/// Write half of [TcpStream].
///
/// The write half of this connection will be shutdown when this value is dropped.
pub struct TcpWriter {
    stream: Rc<net::TcpStream>,
    writable: mpsc::Receiver<()>,
}

assert_not_impl_any!(TcpReader: Send, Sync);

impl Drop for TcpWriter {
    fn drop(&mut self) {
        self.stream.shutdown(std::net::Shutdown::Write).ignore();
    }
}

impl io::Write for TcpWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let stream = Rc::as_ptr(&self.stream) as *mut _;
        TcpStream::write(unsafe { &mut *stream }, &mut self.writable, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl io::Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        TcpStream::read(&mut self.stream, &mut self.readable, buf)
    }
}

impl io::Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        TcpStream::write(&mut self.stream, &mut self.writable, buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
