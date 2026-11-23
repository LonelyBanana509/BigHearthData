//! Frame reader and writer: the length prefix, the version byte and the size limits (wire-protocol
//! spec, decisions 3 and 4). Lane P2.
//!
//! A frame is a `u32` length (big endian), a `u8` version, then the body. The length counts the
//! version byte and the body. No I/O happens here: [`decode`] looks at the bytes a socket has
//! delivered so far, and [`encode`] returns the bytes to write.

use bhdata_types::config::GatewayConfig;

/// The only version this build speaks. A later version may carry a binary body.
pub const VERSION: u8 = 1;

/// Bytes of the length prefix.
pub const PREFIX_LEN: usize = 4;

/// The limits of one connection state (spec, decision 4). There are two sets: before login and after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest frame: the length field, so the version byte counts.
    pub max_frame: u32,
    /// Deepest JSON nesting of arrays and objects.
    pub max_depth: u32,
    /// Most keys in one JSON object.
    pub max_keys: u32,
    /// Longest single string (a key too), in decoded bytes.
    pub max_string: u32,
}

impl Limits {
    /// The limits before login.
    #[must_use]
    pub fn before_login(c: &GatewayConfig) -> Limits {
        Limits {
            max_frame: c.max_frame_before_login,
            max_depth: c.max_json_depth,
            max_keys: c.max_keys_before_login,
            max_string: c.max_string_before_login,
        }
    }

    /// The limits after login.
    #[must_use]
    pub fn after_login(c: &GatewayConfig) -> Limits {
        Limits {
            max_frame: c.max_frame,
            max_depth: c.max_json_depth,
            max_keys: c.max_keys,
            max_string: c.max_string,
        }
    }

    /// Limits for JSON this process wrote itself (a stored result); only the parser's own recursion
    /// cap still applies.
    #[must_use]
    pub fn trusted() -> Limits {
        Limits {
            max_frame: u32::MAX,
            max_depth: 64,
            max_keys: u32::MAX,
            max_string: u32::MAX,
        }
    }
}

/// Why a frame was refused. Each of these closes the connection (spec, decision 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    /// The length field is above the limit. Seen as soon as the prefix arrives, before any body is read.
    #[error("frame of {len} bytes is over the limit of {max}")]
    TooLong {
        /// The length the peer announced.
        len: u32,
        /// The limit in force.
        max: u32,
    },
    /// A length of zero: there is not even room for the version byte.
    #[error("empty frame")]
    Empty,
    /// A version this build does not know.
    #[error("unknown frame version {0}")]
    UnknownVersion(u8),
}

/// Looks for one whole frame at the start of `buf`.
///
/// `Ok(None)` means more bytes are needed. `Ok(Some((body, used)))` is the body of the first frame
/// and how many bytes of `buf` it took, so the caller drops `used` bytes and calls again for the next.
///
/// # Errors
/// [`FrameError`] for a length over `max_frame` (as soon as the four prefix bytes are there), a length
/// of zero, or a version other than [`VERSION`] (as soon as that byte is there).
pub fn decode(buf: &[u8], max_frame: u32) -> Result<Option<(&[u8], usize)>, FrameError> {
    let Some(prefix) = buf.get(..PREFIX_LEN) else {
        return Ok(None);
    };
    let len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    if len == 0 {
        return Err(FrameError::Empty);
    }
    if len > max_frame {
        return Err(FrameError::TooLong {
            len,
            max: max_frame,
        });
    }
    if let Some(&version) = buf.get(PREFIX_LEN) {
        if version != VERSION {
            return Err(FrameError::UnknownVersion(version));
        }
    }
    // `len` is at most `max_frame`; on a 32-bit target a huge limit could still overflow, so add checked.
    let Some(total) = (len as usize).checked_add(PREFIX_LEN) else {
        return Err(FrameError::TooLong {
            len,
            max: max_frame,
        });
    };
    match buf.get(PREFIX_LEN + 1..total) {
        Some(body) => Ok(Some((body, total))),
        None => Ok(None),
    }
}

/// The bytes of one frame carrying `body`.
///
/// # Errors
/// [`FrameError::TooLong`] if the frame would be over `max_frame`.
pub fn encode(body: &[u8], max_frame: u32) -> Result<Vec<u8>, FrameError> {
    let len = u32::try_from(body.len())
        .ok()
        .and_then(|n| n.checked_add(1))
        .filter(|n| *n <= max_frame)
        .ok_or(FrameError::TooLong {
            len: u32::try_from(body.len()).map_or(u32::MAX, |n| n.saturating_add(1)),
            max: max_frame,
        })?;
    let mut out = Vec::with_capacity(PREFIX_LEN + len as usize);
    out.extend_from_slice(&len.to_be_bytes());
    out.push(VERSION);
    out.extend_from_slice(body);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_frame_round_trips() {
        let bytes = encode(b"{\"type\":\"ping\"}", 1_024).unwrap();
        assert_eq!(&bytes[..4], &16u32.to_be_bytes());
        assert_eq!(bytes[4], VERSION);
        let (body, used) = decode(&bytes, 1_024).unwrap().unwrap();
        assert_eq!(body, b"{\"type\":\"ping\"}");
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn the_length_counts_the_version_byte_and_the_body() {
        let bytes = encode(b"abc", 100).unwrap();
        assert_eq!(&bytes[..4], &4u32.to_be_bytes());
    }

    #[test]
    fn a_partial_frame_asks_for_more_at_every_cut() {
        let bytes = encode(b"hello", 100).unwrap();
        for cut in 0..bytes.len() {
            assert_eq!(decode(&bytes[..cut], 100), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn two_frames_in_one_buffer_come_out_one_at_a_time() {
        let mut bytes = encode(b"one", 100).unwrap();
        bytes.extend(encode(b"two", 100).unwrap());
        let (first, used) = decode(&bytes, 100).unwrap().unwrap();
        assert_eq!(first, b"one");
        let (second, used2) = decode(&bytes[used..], 100).unwrap().unwrap();
        assert_eq!(second, b"two");
        assert_eq!(used + used2, bytes.len());
    }

    #[test]
    fn a_length_over_the_limit_is_refused_before_the_body_arrives() {
        let mut bytes = 17u32.to_be_bytes().to_vec();
        bytes.push(VERSION);
        assert_eq!(
            decode(&bytes, 16),
            Err(FrameError::TooLong { len: 17, max: 16 })
        );
        assert_eq!(
            decode(&17u32.to_be_bytes(), 16),
            Err(FrameError::TooLong { len: 17, max: 16 })
        );
        let ok = encode(&[b'x'; 15], 16).unwrap();
        assert!(decode(&ok, 16).unwrap().is_some());
        assert!(encode(&[b'x'; 16], 16).is_err());
    }

    #[test]
    fn a_huge_announced_length_costs_nothing() {
        assert_eq!(
            decode(&u32::MAX.to_be_bytes(), 1_024),
            Err(FrameError::TooLong {
                len: u32::MAX,
                max: 1_024
            })
        );
    }

    #[test]
    fn an_empty_frame_and_an_unknown_version_are_refused() {
        assert_eq!(decode(&0u32.to_be_bytes(), 100), Err(FrameError::Empty));
        let mut bytes = 1u32.to_be_bytes().to_vec();
        bytes.push(2);
        assert_eq!(decode(&bytes, 100), Err(FrameError::UnknownVersion(2)));
        bytes[4] = 0;
        assert_eq!(decode(&bytes, 100), Err(FrameError::UnknownVersion(0)));
    }

    #[test]
    fn limits_come_from_the_gateway_config() {
        let c = GatewayConfig::default();
        let before = Limits::before_login(&c);
        let after = Limits::after_login(&c);
        assert_eq!(before.max_frame, 16 * 1_024);
        assert_eq!(after.max_frame, 1_024 * 1_024);
        assert_eq!((before.max_keys, after.max_keys), (64, 256));
        assert_eq!((before.max_string, after.max_string), (4_096, 65_536));
        assert_eq!(before.max_depth, after.max_depth);
    }

    proptest! {
        #[test]
        fn any_bytes_either_decode_or_ask_for_more_or_are_refused(
            bytes in proptest::collection::vec(any::<u8>(), 0..64),
            max in 1u32..64,
        ) {
            if let Ok(Some((body, used))) = decode(&bytes, max) {
                prop_assert!(used <= bytes.len());
                prop_assert_eq!(used, PREFIX_LEN + 1 + body.len());
                prop_assert!(u32::try_from(1 + body.len()).unwrap() <= max);
            }
        }

        #[test]
        fn encode_then_decode_gives_the_body_back(body in proptest::collection::vec(any::<u8>(), 0..200)) {
            let bytes = encode(&body, 1_000).unwrap();
            let (got, used) = decode(&bytes, 1_000).unwrap().unwrap();
            prop_assert_eq!(got, &body[..]);
            prop_assert_eq!(used, bytes.len());
        }
    }
}
