// Copyright (C) 2021, Cloudflare, Inc.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright notice,
//       this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use std::cmp;

use std::io;

use std::net;

/// For Linux, try to detect GSO is available.
#[cfg(target_os = "linux")]
pub fn detect_gso(socket: &mio::net::UdpSocket, segment_size: usize) -> bool {
    use nix::sys::socket::setsockopt;
    use nix::sys::socket::sockopt::UdpGsoSegment;
    use std::os::unix::io::AsRawFd;

    setsockopt(socket.as_raw_fd(), UdpGsoSegment, &(segment_size as i32)).is_ok()
}

/// For non-Linux, there is no GSO support.
#[cfg(not(target_os = "linux"))]
pub fn detect_gso(_socket: &mio::net::UdpSocket, _segment_size: usize) -> bool {
    false
}

/// Send packets using sendmsg() with GSO.
#[cfg(target_os = "linux")]
fn send_to_gso(
    socket: &mio::net::UdpSocket, buf: &[u8], target: &net::SocketAddr,
    segment_size: usize,
) -> io::Result<usize> {
    use nix::sys::socket::sendmsg;
    use nix::sys::socket::ControlMessage;
    use nix::sys::socket::InetAddr;
    use nix::sys::socket::MsgFlags;
    use nix::sys::socket::SockAddr;
    use nix::sys::uio::IoVec;
    use std::os::unix::io::AsRawFd;

    let iov = [IoVec::from_slice(buf)];
    let segment_size = segment_size as u16;
    let cmsg = ControlMessage::UdpGsoSegments(&segment_size);
    let dst = SockAddr::new_inet(InetAddr::from_std(target));

    match sendmsg(
        socket.as_raw_fd(),
        &iov,
        &[cmsg],
        MsgFlags::empty(),
        Some(&dst),
    ) {
        Ok(v) => Ok(v),
        Err(e) => {
            let e = match e.as_errno() {
                Some(v) => io::Error::from(v),
                None => io::Error::new(io::ErrorKind::Other, e),
            };
            Err(e)
        },
    }
}

/// For non-Linux, there is no GSO support.
#[cfg(not(target_os = "linux"))]
fn send_to_gso(
    _socket: &mio::net::UdpSocket, _buf: &[u8], _target: &net::SocketAddr,
    _segment_size: usize,
) -> io::Result<usize> {
    panic!("send_to_gso() should not be called on non-linux platforms");
}

/// Detecting whether sendmmsg() can be used.
pub fn detect_sendmmsg() -> bool {
    cfg!(target_os = "linux") ||
        cfg!(target_os = "android") ||
        cfg!(target_os = "freebsd") ||
        cfg!(target_os = "netbsd")
}

/// Send packets using sendmmsg().
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
))]
fn send_to_sendmmsg(
    socket: &mio::net::UdpSocket, buf: &[u8], target: &net::SocketAddr,
    segment_size: usize,
) -> io::Result<usize> {
    use nix::sys::socket::sendmmsg;
    use nix::sys::socket::InetAddr;
    use nix::sys::socket::MsgFlags;
    use nix::sys::socket::SendMmsgData;
    use nix::sys::socket::SockAddr;
    use nix::sys::uio::IoVec;
    use std::os::unix::io::AsRawFd;

    let dst = SockAddr::new_inet(InetAddr::from_std(target));

    let mut off = 0;
    let mut left = buf.len();

    let mut msgs = Vec::new();
    let mut iovs = Vec::new();

    while left > 0 {
        let pkt_len = cmp::min(left, segment_size);

        iovs.push([IoVec::from_slice(&buf[off..off + pkt_len])]);

        off += pkt_len;
        left -= pkt_len;
    }

    for iov in iovs.iter() {
        msgs.push(SendMmsgData {
            iov,
            cmsgs: &[],
            addr: Some(dst),
            _lt: Default::default(),
        });
    }

    match sendmmsg(socket.as_raw_fd(), msgs.iter(), MsgFlags::empty()) {
        Ok(results) => Ok(results.iter().sum()),
        Err(e) => match e.as_errno() {
            Some(v) => Err(io::Error::from(v)),
            None => Err(io::Error::new(io::ErrorKind::Other, e)),
        },
    }
}

/// Send packets using sendmmsg().
#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_os = "netbsd",
)))]
fn send_to_sendmmsg(
    _socket: &mio::net::UdpSocket, _buf: &[u8], _target: &net::SocketAddr,
    _segment_size: usize,
) -> io::Result<usize> {
    panic!("send_to_sendmmsg() should not be called on non-supported platforms");
}

/// A wrapper function of send_to().
/// - when GSO enabled, send a packet using send_to_gso().
/// - when sendmmsg() enabled, send a packet using send_to_sendmmsg().
/// Otherwise, send packet using socket.send_to().
pub fn send_to(
    socket: &mio::net::UdpSocket, buf: &[u8], target: &net::SocketAddr,
    segment_size: usize, enable_gso: bool, enable_sendmmsg: bool,
) -> io::Result<usize> {
    if enable_gso {
        match send_to_gso(socket, buf, target, segment_size) {
            Ok(v) => {
                return Ok(v);
            },
            Err(e) => {
                return Err(e);
            },
        }
    }

    if enable_sendmmsg {
        match send_to_sendmmsg(socket, buf, target, segment_size) {
            Ok(v) => {
                return Ok(v);
            },
            Err(e) => {
                return Err(e);
            },
        }
    }

    let mut off = 0;
    let mut left = buf.len();
    let mut written = 0;

    while left > 0 {
        let pkt_len = cmp::min(left, segment_size);

        match socket.send_to(&buf[off..off + pkt_len], target) {
            Ok(v) => {
                written += v;
            },
            Err(e) => return Err(e),
        }

        off += pkt_len;
        left -= pkt_len;
    }

    Ok(written)
}
