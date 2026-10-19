//! Private, single-owner transport for rare server-grab transactions.
//!
//! No tracing, logging, callbacks or x11rb methods are called by these syscall
//! wrappers. The qualified feature graph must also disable x11rb instrumentation.

use std::cell::{Cell, RefCell};
use std::io::{self, IoSlice};
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

use x11rb::rust_connection::{DefaultStream, PollMode, Stream};
use x11rb::utils::RawFdContainer;

const MAX_PACKET: usize = 256 * 1024;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_PACKETS: usize = 256;

struct Framing {
    setup: bool,
    header: [u8; 32],
    filled: usize,
    delivered: usize,
    body: usize,
    validated: bool,
}

impl Framing {
    fn new() -> Self {
        Self { setup: true, header: [0; 32], filled: 0, delivered: 0, body: 0, validated: false }
    }

    fn header_size(&self) -> usize {
        if self.setup {
            8
        } else {
            32
        }
    }

    fn next(&mut self) {
        self.setup = false;
        self.filled = 0;
        self.delivered = 0;
        self.body = 0;
        self.validated = false;
    }
}

/// This is deliberately not shared with any other connection or thread.
pub(super) struct DeadlineStream {
    fd: OwnedFd,
    deadline: Cell<Instant>,
    poisoned: Cell<bool>,
    bytes: Cell<usize>,
    packets: Cell<usize>,
    framing: RefCell<Framing>,
}

impl DeadlineStream {
    pub(super) fn new(stream: DefaultStream) -> Self {
        Self {
            fd: stream.into(),
            deadline: Cell::new(Instant::now() + Duration::from_secs(5)),
            poisoned: Cell::new(false),
            bytes: Cell::new(0),
            packets: Cell::new(0),
            framing: RefCell::new(Framing::new()),
        }
    }

    pub(super) fn begin(&self, deadline: Instant) -> io::Result<()> {
        if self.poisoned.get() {
            return self.check();
        }
        // A previously completed transaction may have been idle longer than its
        // old deadline. Only an observed timeout permanently poisons the socket.
        self.deadline.set(deadline);
        self.bytes.set(0);
        self.packets.set(0);
        self.check()
    }

    /// Only a local shutdown syscall, including during panic unwinding. Never
    /// enters x11rb, takes protocol locks, flushes, logs or waits for the server.
    pub(super) fn close(&self) {
        self.poisoned.set(true);
        // SAFETY: fd is uniquely owned and remains open for this syscall. The
        // socket is nonblocking. shutdown does not access Rust memory.
        unsafe { libc::shutdown(self.fd.as_raw_fd(), libc::SHUT_RDWR) };
    }

    pub(super) fn check(&self) -> io::Result<()> {
        if self.poisoned.get() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "XDND transport closed"));
        }
        if Instant::now() >= self.deadline.get() {
            return self.fail(io::ErrorKind::TimedOut, "XDND transaction deadline");
        }
        Ok(())
    }

    fn fail<T>(&self, kind: io::ErrorKind, message: &'static str) -> io::Result<T> {
        self.close();
        Err(io::Error::new(kind, message))
    }

    fn account(&self, bytes: usize) -> io::Result<()> {
        let total = self.bytes.get().saturating_add(bytes);
        if total > MAX_BYTES {
            return self.fail(io::ErrorKind::InvalidData, "XDND transaction byte budget");
        }
        self.bytes.set(total);
        self.check()
    }

    fn socket_read(&self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            self.check()?;
            let mut iov = libc::iovec { iov_base: out.as_mut_ptr().cast(), iov_len: out.len() };
            // No ancillary buffer: the kernel closes any discarded SCM_RIGHTS
            // descriptors. Core registration never expects received descriptors.
            let mut message = libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: std::ptr::null_mut(),
                msg_controllen: 0,
                msg_flags: 0,
            };
            // SAFETY: iov covers the exclusive output slice for this call; all
            // other pointers are null/zero. The owned FD is a nonblocking socket.
            let result =
                unsafe { libc::recvmsg(self.fd.as_raw_fd(), &mut message, libc::MSG_DONTWAIT) };
            if result >= 0 {
                if message.msg_flags & libc::MSG_CTRUNC != 0 {
                    return self.fail(io::ErrorKind::InvalidData, "unexpected XDND descriptors");
                }
                self.account(result as usize)?;
                if result == 0 {
                    return self.fail(io::ErrorKind::UnexpectedEof, "XDND transport EOF");
                }
                return Ok(result as usize);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Err(error),
                _ => {
                    self.close();
                    return Err(error);
                },
            }
        }
    }
}

impl Stream for DeadlineStream {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        loop {
            self.check()?;
            let framing = self.framing.borrow();
            if mode.readable() && framing.validated && framing.delivered < framing.header_size() {
                return Ok(());
            }
            drop(framing);
            let remaining = self.deadline.get().saturating_duration_since(Instant::now());
            let millis = remaining.as_millis().saturating_add(1).min(i32::MAX as u128) as i32;
            let mut pollfd = libc::pollfd {
                fd: self.fd.as_raw_fd(),
                events: (if mode.readable() { libc::POLLIN } else { 0 })
                    | (if mode.writable() { libc::POLLOUT } else { 0 }),
                revents: 0,
            };
            // SAFETY: pollfd is initialized and valid for one entry. Timeout is
            // finite and every EINTR retry recomputes the absolute deadline.
            let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
            self.check()?;
            if result > 0 {
                return Ok(());
            }
            if result == 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                self.close();
                return Err(error);
            }
        }
    }

    fn read(&self, out: &mut [u8], _fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.check()?;
        if out.is_empty() {
            return Ok(0);
        }
        let mut framing = self.framing.borrow_mut();
        let header_size = framing.header_size();
        if !framing.validated {
            let filled = framing.filled;
            let n = self.socket_read(&mut framing.header[filled..header_size])?;
            framing.filled += n;
            if framing.filled != header_size {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            // Validate before exposing a length field to x11rb's allocator.
            let body = if framing.setup {
                usize::from(u16::from_ne_bytes([framing.header[6], framing.header[7]])) * 4
            } else if matches!(framing.header[0] & 0x7f, 1 | 35) {
                let length = u32::from_ne_bytes(framing.header[4..8].try_into().unwrap());
                usize::try_from(length).ok().and_then(|n| n.checked_mul(4)).unwrap_or(usize::MAX)
            } else {
                0
            };
            let packets = self.packets.get().saturating_add(1);
            if body > MAX_PACKET - header_size || packets > MAX_PACKETS {
                return self.fail(io::ErrorKind::InvalidData, "XDND packet budget");
            }
            self.packets.set(packets);
            framing.body = body;
            framing.validated = true;
        }
        if framing.delivered < header_size {
            let n = out.len().min(header_size - framing.delivered);
            out[..n].copy_from_slice(&framing.header[framing.delivered..framing.delivered + n]);
            framing.delivered += n;
            if framing.delivered == header_size && framing.body == 0 {
                framing.next();
            }
            self.check()?;
            return Ok(n);
        }
        let n = out.len().min(framing.body);
        let n = self.socket_read(&mut out[..n])?;
        framing.body -= n;
        if framing.body == 0 {
            framing.next();
        }
        Ok(n)
    }

    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        if !fds.is_empty() {
            return self.fail(io::ErrorKind::InvalidInput, "XDND descriptor send rejected");
        }
        loop {
            self.check()?;
            // SAFETY: buf remains readable for this syscall. MSG_DONTWAIT and
            // MSG_NOSIGNAL prevent a blocking send and process-level SIGPIPE.
            let n = unsafe {
                libc::send(
                    self.fd.as_raw_fd(),
                    buf.as_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if n >= 0 {
                self.account(n as usize)?;
                return Ok(n as usize);
            }
            let error = io::Error::last_os_error();
            match error.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Err(error),
                _ => {
                    self.close();
                    return Err(error);
                },
            }
        }
    }

    fn write_vectored(
        &self,
        bufs: &[IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.check()?;
        // A partial write of the first nonempty slice follows Stream's contract.
        self.write(bufs.iter().find(|buf| !buf.is_empty()).map_or(&[], |buf| &**buf), fds)
    }
}

impl Drop for DeadlineStream {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    use super::*;

    fn pair() -> (DeadlineStream, UnixStream) {
        let (client, peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let (stream, _) = DefaultStream::from_unix_stream(client).unwrap();
        (DeadlineStream::new(stream), peer)
    }

    #[test]
    fn oversized_reply_is_rejected_before_length_reaches_protocol_allocator() {
        let (stream, mut peer) = pair();
        stream.framing.borrow_mut().setup = false;
        let mut header = [0; 32];
        header[0] = 1;
        header[4..8].copy_from_slice(&u32::MAX.to_ne_bytes());
        peer.write_all(&header).unwrap();
        let mut output = [0xaa; 32];
        assert_eq!(
            stream.read(&mut output, &mut Vec::new()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(output, [0xaa; 32]);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn one_deadline_applies_to_ready_reads_writes_vectored_and_poll() {
        for operation in 0..4 {
            let (stream, mut peer) = pair();
            peer.write_all(&[0; 32]).unwrap();
            stream.deadline.set(Instant::now() - Duration::from_millis(1));
            let error = match operation {
                0 => stream.read(&mut [0; 32], &mut Vec::new()).unwrap_err(),
                1 => stream.write(&[1], &mut Vec::new()).unwrap_err(),
                2 => stream.write_vectored(&[IoSlice::new(&[1])], &mut Vec::new()).unwrap_err(),
                _ => stream.poll(PollMode::ReadAndWritable).unwrap_err(),
            };
            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert!(stream.begin(Instant::now() + Duration::from_secs(1)).is_err());
            // Drain the already-sent peer input is irrelevant to local shutdown.
            assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
        }
    }

    #[test]
    fn partial_headers_are_buffered_and_zero_body_packets_remain_contiguous() {
        let (stream, mut peer) = pair();
        stream.framing.borrow_mut().setup = false;
        let mut header = [0; 32];
        header[0] = 1;
        peer.write_all(&header[..7]).unwrap();
        assert_eq!(
            stream.read(&mut [0; 32], &mut Vec::new()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        peer.write_all(&header[7..]).unwrap();
        let mut first = [0; 8];
        assert_eq!(stream.read(&mut first, &mut Vec::new()).unwrap(), 8);
        assert_eq!(first, header[..8]);
        stream.poll(PollMode::Readable).unwrap();
        let mut rest = [0; 24];
        assert_eq!(stream.read(&mut rest, &mut Vec::new()).unwrap(), 24);
        assert_eq!(rest, header[8..]);
        assert!(!stream.framing.borrow().validated);
    }

    #[test]
    fn packet_budget_closes_an_unsolicited_event_flood() {
        let (stream, mut peer) = pair();
        stream.framing.borrow_mut().setup = false;
        let mut event = [0; 32];
        event[0] = 12;
        for _ in 0..MAX_PACKETS {
            peer.write_all(&event).unwrap();
            assert_eq!(stream.read(&mut [0; 32], &mut Vec::new()).unwrap(), 32);
        }
        peer.write_all(&event).unwrap();
        assert_eq!(
            stream.read(&mut [0; 32], &mut Vec::new()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn active_poll_expires_and_shutdown_does_not_wait_for_peer() {
        let (stream, mut peer) = pair();
        stream.begin(Instant::now() + Duration::from_millis(10)).unwrap();
        assert_eq!(stream.poll(PollMode::Readable).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }
}
