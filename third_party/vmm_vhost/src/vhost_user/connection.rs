// Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Common data structures for listener and endpoint.

mod socket;

pub use self::socket::SocketListener;

use std::fs::File;
use std::marker::PhantomData;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use data_model::DataInit;
use libc::{c_void, iovec};
use sys_util::ScmSocket;

use super::message::*;
use super::{Error, Result};

/// Listener for accepting connections.
pub trait Listener: Sized {
    /// Accept an incoming connection.
    fn accept(&self) -> Result<Option<UnixStream>>;

    /// Change blocking status on the listener.
    fn set_nonblocking(&self, block: bool) -> Result<()>;
}

/// Unix domain socket endpoint for vhost-user connection.
pub(super) struct Endpoint<R: Req> {
    sock: UnixStream,
    _r: PhantomData<R>,
}

impl<R: Req> Endpoint<R> {
    /// Create a new stream by connecting to server at `str`.
    ///
    /// # Return:
    /// * - the new Endpoint object on success.
    /// * - SocketConnect: failed to connect to peer.
    pub fn connect<P: AsRef<Path>>(path: P) -> Result<Self> {
        let sock = UnixStream::connect(path).map_err(Error::SocketConnect)?;
        Ok(Self::from_stream(sock))
    }

    /// Create an endpoint from a stream object.
    pub fn from_stream(sock: UnixStream) -> Self {
        Endpoint {
            sock,
            _r: PhantomData,
        }
    }

    /// Sends bytes from scatter-gather vectors over the socket with optional attached file
    /// descriptors.
    ///
    /// # Return:
    /// * - number of bytes sent on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    pub fn send_iovec(&mut self, iovs: &[&[u8]], fds: Option<&[RawFd]>) -> Result<usize> {
        let rfds = match fds {
            Some(rfds) => rfds,
            _ => &[],
        };
        self.sock.send_bufs_with_fds(iovs, rfds).map_err(Into::into)
    }

    /// Sends all bytes from scatter-gather vectors over the socket with optional attached file
    /// descriptors. Will loop until all data has been transfered.
    ///
    /// # Return:
    /// * - number of bytes sent on success
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    pub fn send_iovec_all(
        &mut self,
        mut iovs: &mut [&[u8]],
        mut fds: Option<&[RawFd]>,
    ) -> Result<usize> {
        // Guarantee that `iovs` becomes empty if it doesn't contain any data.
        advance_slices(&mut iovs, 0);

        let mut data_sent = 0;
        while !iovs.is_empty() {
            match self.send_iovec(iovs, fds) {
                Ok(0) => {
                    break;
                }
                Ok(n) => {
                    data_sent += n;
                    fds = None;
                    advance_slices(&mut iovs, n);
                }
                Err(e) => match e {
                    Error::SocketRetry(_) => {}
                    _ => return Err(e),
                },
            }
        }
        Ok(data_sent)
    }

    /// Sends bytes from a slice over the socket with optional attached file descriptors.
    ///
    /// # Return:
    /// * - number of bytes sent on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    #[cfg(test)]
    pub fn send_slice(&mut self, data: &[u8], fds: Option<&[RawFd]>) -> Result<usize> {
        self.send_iovec(&[data], fds)
    }

    /// Sends a header-only message with optional attached file descriptors.
    ///
    /// # Return:
    /// * - number of bytes sent on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - PartialMessage: received a partial message.
    pub fn send_header(
        &mut self,
        hdr: &VhostUserMsgHeader<R>,
        fds: Option<&[RawFd]>,
    ) -> Result<()> {
        let mut iovs = [hdr.as_slice()];
        let bytes = self.send_iovec_all(&mut iovs[..], fds)?;
        if bytes != mem::size_of::<VhostUserMsgHeader<R>>() {
            return Err(Error::PartialMessage);
        }
        Ok(())
    }

    /// Send a message with header and body. Optional file descriptors may be attached to
    /// the message.
    ///
    /// # Return:
    /// * - number of bytes sent on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - PartialMessage: received a partial message.
    pub fn send_message<T: Sized + DataInit>(
        &mut self,
        hdr: &VhostUserMsgHeader<R>,
        body: &T,
        fds: Option<&[RawFd]>,
    ) -> Result<()> {
        if mem::size_of::<T>() > MAX_MSG_SIZE {
            return Err(Error::OversizedMsg);
        }
        let mut iovs = [hdr.as_slice(), body.as_slice()];
        let bytes = self.send_iovec_all(&mut iovs[..], fds)?;
        if bytes != mem::size_of::<VhostUserMsgHeader<R>>() + mem::size_of::<T>() {
            return Err(Error::PartialMessage);
        }
        Ok(())
    }

    /// Send a message with header, body and payload. Optional file descriptors
    /// may also be attached to the message.
    ///
    /// # Return:
    /// * - number of bytes sent on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - OversizedMsg: message size is too big.
    /// * - PartialMessage: received a partial message.
    /// * - IncorrectFds: wrong number of attached fds.
    pub fn send_message_with_payload<T: Sized + DataInit>(
        &mut self,
        hdr: &VhostUserMsgHeader<R>,
        body: &T,
        payload: &[u8],
        fds: Option<&[RawFd]>,
    ) -> Result<()> {
        let len = payload.len();
        if mem::size_of::<T>() > MAX_MSG_SIZE {
            return Err(Error::OversizedMsg);
        }
        if len > MAX_MSG_SIZE - mem::size_of::<T>() {
            return Err(Error::OversizedMsg);
        }
        if let Some(fd_arr) = fds {
            if fd_arr.len() > MAX_ATTACHED_FD_ENTRIES {
                return Err(Error::IncorrectFds);
            }
        }

        let mut iovs = [hdr.as_slice(), body.as_slice(), payload];
        let total = mem::size_of::<VhostUserMsgHeader<R>>() + mem::size_of::<T>() + len;
        let len = self.send_iovec_all(&mut iovs, fds)?;
        if len != total {
            return Err(Error::PartialMessage);
        }
        Ok(())
    }

    /// Reads bytes from the socket into the given scatter/gather vectors.
    ///
    /// # Return:
    /// * - (number of bytes received, buf) on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    pub fn recv_data(&mut self, len: usize) -> Result<(usize, Vec<u8>)> {
        let mut rbuf = vec![0u8; len];
        let (bytes, _) = self.sock.recv_with_fds(&mut rbuf[..], &mut [])?;
        Ok((bytes, rbuf))
    }

    /// Reads bytes from the socket into the given scatter/gather vectors with optional attached
    /// file.
    ///
    /// The underlying communication channel is a Unix domain socket in STREAM mode. It's a little
    /// tricky to pass file descriptors through such a communication channel. Let's assume that a
    /// sender sending a message with some file descriptors attached. To successfully receive those
    /// attached file descriptors, the receiver must obey following rules:
    ///   1) file descriptors are attached to a message.
    ///   2) message(packet) boundaries must be respected on the receive side.
    /// In other words, recvmsg() operations must not cross the packet boundary, otherwise the
    /// attached file descriptors will get lost.
    /// Note that this function wraps received file descriptors as `File`.
    ///
    /// # Return:
    /// * - (number of bytes received, [received files]) on success
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    pub fn recv_into_iovec(&mut self, iovs: &mut [iovec]) -> Result<(usize, Option<Vec<File>>)> {
        let mut fd_array = vec![0; MAX_ATTACHED_FD_ENTRIES];
        let (bytes, fds) = self.sock.recv_iovecs_with_fds(iovs, &mut fd_array)?;

        let files = match fds {
            0 => None,
            n => {
                let files = fd_array
                    .iter()
                    .take(n)
                    .map(|fd| {
                        // Safe because we have the ownership of `fd`.
                        unsafe { File::from_raw_fd(*fd) }
                    })
                    .collect();
                Some(files)
            }
        };

        Ok((bytes, files))
    }

    /// Reads all bytes from the socket into the given scatter/gather vectors with optional
    /// attached files. Will loop until all data has been transferred.
    ///
    /// The underlying communication channel is a Unix domain socket in STREAM mode. It's a little
    /// tricky to pass file descriptors through such a communication channel. Let's assume that a
    /// sender sending a message with some file descriptors attached. To successfully receive those
    /// attached file descriptors, the receiver must obey following rules:
    ///   1) file descriptors are attached to a message.
    ///   2) message(packet) boundaries must be respected on the receive side.
    /// In other words, recvmsg() operations must not cross the packet boundary, otherwise the
    /// attached file descriptors will get lost.
    /// Note that this function wraps received file descriptors as `File`.
    ///
    /// # Return:
    /// * - (number of bytes received, [received fds]) on success
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    pub fn recv_into_iovec_all(
        &mut self,
        iovs: &mut [iovec],
    ) -> Result<(usize, Option<Vec<File>>)> {
        let mut data_read = 0;
        let mut data_total = 0;
        let mut rfds = None;
        let iov_lens: Vec<usize> = iovs.iter().map(|iov| iov.iov_len).collect();
        for len in &iov_lens {
            data_total += len;
        }

        while (data_total - data_read) > 0 {
            let (nr_skip, offset) = get_sub_iovs_offset(&iov_lens, data_read);
            let iov = &mut iovs[nr_skip];

            let mut data = [
                &[iovec {
                    iov_base: (iov.iov_base as usize + offset) as *mut c_void,
                    iov_len: iov.iov_len - offset,
                }],
                &iovs[(nr_skip + 1)..],
            ]
            .concat();

            let res = self.recv_into_iovec(&mut data);
            match res {
                Ok((0, _)) => return Ok((data_read, rfds)),
                Ok((n, fds)) => {
                    if data_read == 0 {
                        rfds = fds;
                    }
                    data_read += n;
                }
                Err(e) => match e {
                    Error::SocketRetry(_) => {}
                    _ => return Err(e),
                },
            }
        }
        Ok((data_read, rfds))
    }

    /// Reads bytes from the socket into a new buffer with optional attached
    /// files. Received file descriptors are set close-on-exec and converted to `File`.
    ///
    /// # Return:
    /// * - (number of bytes received, buf, [received files]) on success.
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    #[cfg(test)]
    pub fn recv_into_buf(
        &mut self,
        buf_size: usize,
    ) -> Result<(usize, Vec<u8>, Option<Vec<File>>)> {
        let mut buf = vec![0u8; buf_size];
        let (bytes, files) = {
            let mut iovs = [iovec {
                iov_base: buf.as_mut_ptr() as *mut c_void,
                iov_len: buf_size,
            }];
            self.recv_into_iovec(&mut iovs)?
        };
        Ok((bytes, buf, files))
    }

    /// Receive a header-only message with optional attached files.
    /// Note, only the first MAX_ATTACHED_FD_ENTRIES file descriptors will be
    /// accepted and all other file descriptor will be discard silently.
    ///
    /// # Return:
    /// * - (message header, [received files]) on success.
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - PartialMessage: received a partial message.
    /// * - InvalidMessage: received a invalid message.
    pub fn recv_header(&mut self) -> Result<(VhostUserMsgHeader<R>, Option<Vec<File>>)> {
        let mut hdr = VhostUserMsgHeader::default();
        let mut iovs = [iovec {
            iov_base: (&mut hdr as *mut VhostUserMsgHeader<R>) as *mut c_void,
            iov_len: mem::size_of::<VhostUserMsgHeader<R>>(),
        }];
        let (bytes, files) = self.recv_into_iovec_all(&mut iovs[..])?;

        if bytes != mem::size_of::<VhostUserMsgHeader<R>>() {
            return Err(Error::PartialMessage);
        } else if !hdr.is_valid() {
            return Err(Error::InvalidMessage);
        }

        Ok((hdr, files))
    }

    /// Receive a message with optional attached file descriptors.
    /// Note, only the first MAX_ATTACHED_FD_ENTRIES file descriptors will be
    /// accepted and all other file descriptor will be discard silently.
    ///
    /// # Return:
    /// * - (message header, message body, [received files]) on success.
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - PartialMessage: received a partial message.
    /// * - InvalidMessage: received a invalid message.
    pub fn recv_body<T: Sized + DataInit + Default + VhostUserMsgValidator>(
        &mut self,
    ) -> Result<(VhostUserMsgHeader<R>, T, Option<Vec<File>>)> {
        let mut hdr = VhostUserMsgHeader::default();
        let mut body: T = Default::default();
        let mut iovs = [
            iovec {
                iov_base: (&mut hdr as *mut VhostUserMsgHeader<R>) as *mut c_void,
                iov_len: mem::size_of::<VhostUserMsgHeader<R>>(),
            },
            iovec {
                iov_base: (&mut body as *mut T) as *mut c_void,
                iov_len: mem::size_of::<T>(),
            },
        ];
        let (bytes, files) = self.recv_into_iovec_all(&mut iovs[..])?;

        let total = mem::size_of::<VhostUserMsgHeader<R>>() + mem::size_of::<T>();
        if bytes != total {
            return Err(Error::PartialMessage);
        } else if !hdr.is_valid() || !body.is_valid() {
            return Err(Error::InvalidMessage);
        }

        Ok((hdr, body, files))
    }

    /// Receive a message with header and optional content. Callers need to
    /// pre-allocate a big enough buffer to receive the message body and
    /// optional payload. If there are attached file descriptor associated
    /// with the message, the first MAX_ATTACHED_FD_ENTRIES file descriptors
    /// will be accepted and all other file descriptor will be discard
    /// silently.
    ///
    /// # Return:
    /// * - (message header, message size, [received files]) on success.
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - PartialMessage: received a partial message.
    /// * - InvalidMessage: received a invalid message.
    #[cfg(test)]
    pub fn recv_body_into_buf(
        &mut self,
        buf: &mut [u8],
    ) -> Result<(VhostUserMsgHeader<R>, usize, Option<Vec<File>>)> {
        let mut hdr = VhostUserMsgHeader::default();
        let mut iovs = [
            iovec {
                iov_base: (&mut hdr as *mut VhostUserMsgHeader<R>) as *mut c_void,
                iov_len: mem::size_of::<VhostUserMsgHeader<R>>(),
            },
            iovec {
                iov_base: buf.as_mut_ptr() as *mut c_void,
                iov_len: buf.len(),
            },
        ];
        let (bytes, files) = self.recv_into_iovec_all(&mut iovs[..])?;

        if bytes < mem::size_of::<VhostUserMsgHeader<R>>() {
            return Err(Error::PartialMessage);
        } else if !hdr.is_valid() {
            return Err(Error::InvalidMessage);
        }

        Ok((hdr, bytes - mem::size_of::<VhostUserMsgHeader<R>>(), files))
    }

    /// Receive a message with optional payload and attached file descriptors.
    /// Note, only the first MAX_ATTACHED_FD_ENTRIES file descriptors will be
    /// accepted and all other file descriptor will be discard silently.
    ///
    /// # Return:
    /// * - (message header, message body, size of payload, [received files]) on success.
    /// * - SocketRetry: temporary error caused by signals or short of resources.
    /// * - SocketBroken: the underline socket is broken.
    /// * - SocketError: other socket related errors.
    /// * - PartialMessage: received a partial message.
    /// * - InvalidMessage: received a invalid message.
    #[cfg_attr(feature = "cargo-clippy", allow(clippy::type_complexity))]
    pub fn recv_payload_into_buf<T: Sized + DataInit + Default + VhostUserMsgValidator>(
        &mut self,
        buf: &mut [u8],
    ) -> Result<(VhostUserMsgHeader<R>, T, usize, Option<Vec<File>>)> {
        let mut hdr = VhostUserMsgHeader::default();
        let mut body: T = Default::default();
        let mut iovs = [
            iovec {
                iov_base: (&mut hdr as *mut VhostUserMsgHeader<R>) as *mut c_void,
                iov_len: mem::size_of::<VhostUserMsgHeader<R>>(),
            },
            iovec {
                iov_base: (&mut body as *mut T) as *mut c_void,
                iov_len: mem::size_of::<T>(),
            },
            iovec {
                iov_base: buf.as_mut_ptr() as *mut c_void,
                iov_len: buf.len(),
            },
        ];
        let (bytes, files) = self.recv_into_iovec_all(&mut iovs[..])?;

        let total = mem::size_of::<VhostUserMsgHeader<R>>() + mem::size_of::<T>();
        if bytes < total {
            return Err(Error::PartialMessage);
        } else if !hdr.is_valid() || !body.is_valid() {
            return Err(Error::InvalidMessage);
        }

        Ok((hdr, body, bytes - total, files))
    }
}

impl<T: Req> AsRawFd for Endpoint<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }
}

// Given a slice of sizes and the `skip_size`, return the offset of `skip_size` in the slice.
// For example:
//     let iov_lens = vec![4, 4, 5];
//     let size = 6;
//     assert_eq!(get_sub_iovs_offset(&iov_len, size), (1, 2));
fn get_sub_iovs_offset(iov_lens: &[usize], skip_size: usize) -> (usize, usize) {
    let mut size = skip_size;
    let mut nr_skip = 0;

    for len in iov_lens {
        if size >= *len {
            size -= *len;
            nr_skip += 1;
        } else {
            break;
        }
    }
    (nr_skip, size)
}

// Advance the internal cursor of the slices.
// This is same with a nightly API `IoSlice::advance_slices` but for `[&[u8]]`.
fn advance_slices(bufs: &mut &mut [&[u8]], mut count: usize) {
    use std::mem::replace;

    let mut idx = 0;
    for b in bufs.iter() {
        if count < b.len() {
            break;
        }
        count -= b.len();
        idx += 1;
    }
    *bufs = &mut replace(bufs, &mut [])[idx..];
    if !bufs.is_empty() {
        bufs[0] = &bufs[0][count..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_advance_slices() {
        // Test case from https://doc.rust-lang.org/std/io/struct.IoSlice.html#method.advance_slices
        let buf1 = [1; 8];
        let buf2 = [2; 16];
        let buf3 = [3; 8];
        let mut bufs = &mut [&buf1[..], &buf2[..], &buf3[..]][..];
        advance_slices(&mut bufs, 10);
        assert_eq!(bufs[0], [2; 14].as_ref());
        assert_eq!(bufs[1], [3; 8].as_ref());
    }
}
