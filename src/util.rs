//! Useful traits and other utilities that don't really belong anywhere else.
use std::{
    io::{self, Read, Write},
    time::Duration,
};

use bytes::BytesMut;
use usb_gadget::function::custom::EndpointReceiver;

pub trait ReadExt {
    /// Tries to read exactly `read_len` bytes, like `read_exact`, but unlike `read_exact`, is
    /// forgiving of unexpected EOF.
    ///
    /// The returned vector will have exactly `read_len` bytes appended, unless an EOF was
    /// encountered, in which case it will have strictly shorter than `read_len` new bytes added.
    fn read_to_vec(&mut self, vec: &mut Vec<u8>, read_len: usize) -> io::Result<()>;
}

impl<T: Read> ReadExt for T {
    fn read_to_vec(&mut self, vec: &mut Vec<u8>, read_len: usize) -> io::Result<()> {
        const CHUNK_SIZE: usize = 65536;

        let read_len = read_len + vec.len();
        let mut cursor = vec.len();
        loop {
            assert!(cursor <= read_len);
            if cursor == read_len {
                // All requested reading has been done.
                assert_eq!(vec.len(), read_len);
                return Ok(());
            }

            // Make room for the next chunk
            vec.resize(std::cmp::min(read_len, cursor + CHUNK_SIZE), 0u8);

            // Perform the read, handle errors, and advance `cursor`
            cursor += match self.read(&mut vec[cursor..]) {
                // This is an EOF; it means the final read size is `cursor`
                Ok(0) => {
                    vec.truncate(cursor);
                    return Ok(());
                }

                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => 0,
                Err(x) => return Err(x),
            };
        }
    }
}

/// This struct wraps a [`std::sync::mpsc::Receiver`] and transforms that
/// exposes a [`std::io::Read`] interface.
pub struct ReceiverReader<'a> {
    receiver: &'a mut EndpointReceiver,
    buffer: Vec<u8>,
    timeout: Option<Duration>,
}

impl<'a> ReceiverReader<'a> {
    pub fn new(receiver: &'a mut EndpointReceiver, timeout: Option<Duration>) -> Self {
        Self {
            receiver,
            buffer: Vec::new(),
            timeout,
        }
    }

    pub fn push_to_buffer(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    pub fn take_buffered_bytes(&mut self, read_buf: &mut [u8]) -> usize {
        let len = self.buffer.len().min(read_buf.len());
        if len > 0 {
            let data: Vec<u8> = self.buffer.drain(..len).collect();
            read_buf[..len].copy_from_slice(&data);
        }
        len
    }
}

impl<'a> io::Read for ReceiverReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let total_len = buf.len();
        let bytes_read = self.take_buffered_bytes(buf);
        let mut cursor = io::Cursor::new(buf);
        cursor.set_position(bytes_read as u64);

        while cursor.position() < total_len as u64 {
            let buffer = BytesMut::with_capacity(total_len);
            println!("bufferen kut");
            let recv_result = if let Some(timeout) = self.timeout {
                self.receiver.recv_and_fetch_timeout(buffer, timeout)
            } else {
                self.receiver.recv_and_fetch(buffer).map_err(broken_pipe)
            };

            match recv_result {
                Ok(bytes) => {
                    let len = cursor.write(&bytes)?;
                    if len < bytes.len() {
                        self.push_to_buffer(&bytes[len..]);
                    }
                }
                Err(e) if cursor.position() == 0 => {
                    return Err(e);
                }
                // since we read on the internal buffer, result is an Ok response even
                // though the underlying receiver closed.
                _ => return Ok(cursor.position() as usize),
            }
        }

        Ok(cursor.position() as usize)
    }
}

fn broken_pipe<T>(_err: T) -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "channel closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc::channel, thread};

    #[test]
    fn receive_once_and_drain_buffer_test() {
        let (sender, receiver) = channel::<Vec<u8>>();
        let mut rr = ReceiverReader::new(receiver, None);
        sender.send(vec![1, 2]).unwrap();
        drop(sender);

        let mut buffer = [0u8; 5];
        rr.read(&mut buffer[0..1]).unwrap();
        assert_eq!(buffer[0], 1);
        rr.read(&mut buffer[0..1]).unwrap();
        assert_eq!(buffer[0], 2);
        assert!(rr.read(&mut buffer[0..1]).is_err());
    }

    #[test]
    fn drain_buffer_and_new_read_available_test() {
        let (sender, receiver) = channel::<Vec<u8>>();
        let mut rr = ReceiverReader::new(receiver, None);
        sender.send(vec![1, 2]).unwrap();

        let mut buffer = [0u8; 5];
        rr.read(&mut buffer[0..1]).unwrap();
        assert_eq!(buffer[0], 1);
        rr.read(&mut buffer[0..1]).unwrap();
        assert_eq!(buffer[0], 2);
        sender.send(vec![8, 9]).unwrap();
        rr.read(&mut buffer[0..2]).unwrap();
        assert_eq!(vec![8, 9], buffer[0..2]);
    }

    #[test]
    fn exhaust_reader_return_result() {
        let (sender, receiver) = channel::<Vec<u8>>();
        let result = thread::spawn(|| {
            let mut rr = ReceiverReader::new(receiver, None);
            let mut buffer = [0u8; 5];
            rr.read(&mut buffer).unwrap();
            assert_eq!(vec![1, 2, 3, 4, 5], buffer);
            rr
        });

        sender.send(vec![1, 2]).unwrap();
        sender.send(vec![3, 4, 5, 6, 7]).unwrap();
        let mut rr = result.join().unwrap();
        drop(sender);

        let mut buffer = [0u8; 4];
        let res = rr.read(&mut buffer).unwrap();
        // we have exhaust the channel unable to complete the whole 4 bytes
        // read request. return the last available bytes
        assert_eq!(vec![6, 7], buffer[0..2]);
        assert_eq!(res, 2);
    }

    #[test]
    fn test_read_to_vec() -> io::Result<()> {
        let mut vec = Vec::new();
        io::repeat(0xAA).read_to_vec(&mut vec, 4)?;
        assert_eq!(vec, [0xAA; 4]);
        io::repeat(0xBB).read_to_vec(&mut vec, 2)?;
        assert_eq!(vec, [0xAA, 0xAA, 0xAA, 0xAA, 0xBB, 0xBB]);
        (&[1, 2, 3][..]).read_to_vec(&mut vec, 8)?;
        assert_eq!(vec, [0xAA, 0xAA, 0xAA, 0xAA, 0xBB, 0xBB, 1, 2, 3]);
        Ok(())
    }
}
