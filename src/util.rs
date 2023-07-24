//! Useful traits and other utilities that don't really belong anywhere else.

use std::io::{self, Read};

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
