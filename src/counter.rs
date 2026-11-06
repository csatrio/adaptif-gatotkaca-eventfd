//! 8-byte buffer encoding shared by every backend's `io::Read`/`io::Write`.
//!
//! The eventfd rules themselves live in the kernel (native backend) or in
//! `src/sys/shm.rs` (unix and Windows shims).

use std::io;

use crate::error;

/// Decodes an eventfd write buffer (`EINVAL` if shorter than 8 bytes).
pub(crate) fn decode(buf: &[u8]) -> io::Result<u64> {
    match buf.get(..8) {
        Some(b) => Ok(u64::from_ne_bytes(b.try_into().expect("8 bytes"))),
        None => Err(error::invalid_input()),
    }
}

/// Encodes a read result into `buf` (`EINVAL` if shorter than 8 bytes).
pub(crate) fn encode(value: u64, buf: &mut [u8]) -> io::Result<usize> {
    match buf.get_mut(..8) {
        Some(b) => {
            b.copy_from_slice(&value.to_ne_bytes());
            Ok(8)
        }
        None => Err(error::invalid_input()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffers() {
        assert!(decode(&[0; 7]).is_err());
        assert_eq!(decode(&5u64.to_ne_bytes()).unwrap(), 5);
        let mut b = [0u8; 8];
        assert!(encode(1, &mut b[..4]).is_err());
        assert_eq!(encode(9, &mut b).unwrap(), 8);
        assert_eq!(u64::from_ne_bytes(b), 9);
    }
}
