#![no_main]
//! The frame reader: any bytes either give a frame, ask for more, or are refused. Never a panic, and
//! a frame that decodes is exactly the bytes it says it is.

use bhdata_proto::frame::{decode, encode, PREFIX_LEN};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The first byte picks a small limit so the limit paths are reached; the rest is the stream.
    let Some((&pick, stream)) = data.split_first() else {
        return;
    };
    let max_frame = match pick % 4 {
        0 => 16,
        1 => 16 * 1_024,
        2 => 1_024 * 1_024,
        _ => u32::MAX,
    };
    if let Ok(Some((body, used))) = decode(stream, max_frame) {
        assert!(used <= stream.len());
        assert_eq!(used, PREFIX_LEN + 1 + body.len());
        // What was read can be written back identically.
        let again = encode(body, max_frame).expect("a frame that was read fits its own limit");
        assert_eq!(&again[..], &stream[..used]);
    }
});
