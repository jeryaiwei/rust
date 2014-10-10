// Copyright 2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use io::net::ip;
use io::IoResult;
use libc;
use mem;
use ptr;
use prelude::*;
use super::{last_error, last_net_error, retry, sock_t};
use sync::{Arc, atomic};
use sys::fs::FileDesc;
use sys::{mod, c, set_nonblocking, wouldblock, timer};
use sys_common::{mod, timeout, eof};
use sys_common::net::*;

pub use sys_common::net::TcpStream;

pub struct Event(c::WSAEVENT);

impl Event {
    pub fn new() -> IoResult<Event> {
        let event = unsafe { c::WSACreateEvent() };
        if event == c::WSA_INVALID_EVENT {
            Err(super::last_error())
        } else {
            Ok(Event(event))
        }
    }

    pub fn handle(&self) -> c::WSAEVENT { let Event(handle) = *self; handle }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { let _ = c::WSACloseEvent(self.handle()); }
    }
}

////////////////////////////////////////////////////////////////////////////////
// TCP listeners
////////////////////////////////////////////////////////////////////////////////

pub struct TcpListener {
    inner: FileDesc,
}

impl TcpListener {
    pub fn bind(addr: ip::SocketAddr) -> IoResult<TcpListener> {
        sys::init_net();

        let fd = try!(socket(addr, libc::SOCK_STREAM));
        let ret = TcpListener { inner: FileDesc::new(fd as libc::c_int, true) };

        let mut storage = unsafe { mem::zeroed() };
        let len = addr_to_sockaddr(addr, &mut storage);
        let addrp = &storage as *const _ as *const libc::sockaddr;

        match unsafe { libc::bind(fd, addrp, len) } {
            -1 => Err(last_net_error()),
            _ => Ok(ret),
        }
    }

    pub fn fd(&self) -> sock_t { self.inner.fd as sock_t }

    pub fn listen(self, backlog: int) -> IoResult<TcpAcceptor> {
        match unsafe { libc::listen(self.fd(), backlog as libc::c_int) } {
            -1 => Err(last_net_error()),

            _ => {
                let accept = try!(Event::new());
                let ret = unsafe {
                    c::WSAEventSelect(self.fd(), accept.handle(), c::FD_ACCEPT)
                };
                if ret != 0 {
                    return Err(last_net_error())
                }
                Ok(TcpAcceptor {
                    inner: Arc::new(AcceptorInner {
                        listener: self,
                        abort: try!(Event::new()),
                        accept: accept,
                        closed: atomic::AtomicBool::new(false),
                    }),
                    deadline: 0,
                })
            }
        }
    }

    pub fn socket_name(&mut self) -> IoResult<ip::SocketAddr> {
        sockname(self.fd(), libc::getsockname)
    }
}

pub struct TcpAcceptor {
    inner: Arc<AcceptorInner>,
    deadline: u64,
}

struct AcceptorInner {
    listener: TcpListener,
    abort: Event,
    accept: Event,
    closed: atomic::AtomicBool,
}

impl TcpAcceptor {
    pub fn fd(&self) -> sock_t { self.inner.listener.fd() }

    pub fn accept(&mut self) -> IoResult<TcpStream> {
        // Unlink unix, windows cannot invoke `select` on arbitrary file
        // descriptors like pipes, only sockets. Consequently, windows cannot
        // use the same implementation as unix for accept() when close_accept()
        // is considered.
        //
        // In order to implement close_accept() and timeouts, windows uses
        // event handles. An acceptor-specific abort event is created which
        // will only get set in close_accept(), and it will never be un-set.
        // Additionally, another acceptor-specific event is associated with the
        // FD_ACCEPT network event.
        //
        // These two events are then passed to WaitForMultipleEvents to see
        // which one triggers first, and the timeout passed to this function is
        // the local timeout for the acceptor.
        //
        // If the wait times out, then the accept timed out. If the wait
        // succeeds with the abort event, then we were closed, and if the wait
        // succeeds otherwise, then we do a nonblocking poll via `accept` to
        // see if we can accept a connection. The connection is candidate to be
        // stolen, so we do all of this in a loop as well.
        let events = [self.inner.abort.handle(), self.inner.accept.handle()];

        while !self.inner.closed.load(atomic::SeqCst) {
            let ms = if self.deadline == 0 {
                c::WSA_INFINITE as u64
            } else {
                let now = timer::now();
                if self.deadline < now {0} else {self.deadline - now}
            };
            let ret = unsafe {
                c::WSAWaitForMultipleEvents(2, events.as_ptr(), libc::FALSE,
                                            ms as libc::DWORD, libc::FALSE)
            };
            match ret {
                c::WSA_WAIT_TIMEOUT => {
                    return Err(timeout("accept timed out"))
                }
                c::WSA_WAIT_FAILED => return Err(last_net_error()),
                c::WSA_WAIT_EVENT_0 => break,
                n => assert_eq!(n, c::WSA_WAIT_EVENT_0 + 1),
            }

            let mut wsaevents: c::WSANETWORKEVENTS = unsafe { mem::zeroed() };
            let ret = unsafe {
                c::WSAEnumNetworkEvents(self.fd(), events[1], &mut wsaevents)
            };
            if ret != 0 { return Err(last_net_error()) }

            if wsaevents.lNetworkEvents & c::FD_ACCEPT == 0 { continue }
            match unsafe {
                libc::accept(self.fd(), ptr::null_mut(), ptr::null_mut())
            } {
                -1 if wouldblock() => {}
                -1 => return Err(last_net_error()),

                // Accepted sockets inherit the same properties as the caller,
                // so we need to deregister our event and switch the socket back
                // to blocking mode
                fd => {
                    let stream = TcpStream::new(fd);
                    let ret = unsafe {
                        c::WSAEventSelect(fd, events[1], 0)
                    };
                    if ret != 0 { return Err(last_net_error()) }
                    try!(set_nonblocking(fd, false));
                    return Ok(stream)
                }
            }
        }

        Err(eof())
    }

    pub fn socket_name(&mut self) -> IoResult<ip::SocketAddr> {
        sockname(self.fd(), libc::getsockname)
    }

    pub fn set_timeout(&mut self, timeout: Option<u64>) {
        self.deadline = timeout.map(|a| timer::now() + a).unwrap_or(0);
    }

    pub fn close_accept(&mut self) -> IoResult<()> {
        self.inner.closed.store(true, atomic::SeqCst);
        let ret = unsafe { c::WSASetEvent(self.inner.abort.handle()) };
        if ret == libc::TRUE {
            Ok(())
        } else {
            Err(last_net_error())
        }
    }
}

impl Clone for TcpAcceptor {
    fn clone(&self) -> TcpAcceptor {
        TcpAcceptor {
            inner: self.inner.clone(),
            deadline: 0,
        }
    }
}
