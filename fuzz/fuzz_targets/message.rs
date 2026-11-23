#![no_main]
//! The message codec: any body is a message or an error, never a panic; a message that decoded
//! encodes to a body that decodes to the same message.

use bhdata_proto::frame::Limits;
use bhdata_proto::message::{Codec, JsonCodec};
use bhdata_types::config::GatewayConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let config = GatewayConfig::default();
    for limits in [Limits::before_login(&config), Limits::after_login(&config)] {
        if let Ok(msg) = JsonCodec.decode_client(data, &limits) {
            let body = JsonCodec.encode_client(&msg);
            let again = JsonCodec
                .decode_client(&body, &Limits::trusted())
                .expect("an encoded client message decodes");
            assert_eq!(again, msg);
        }
        if let Ok(msg) = JsonCodec.decode_server(data, &limits) {
            let body = JsonCodec.encode_server(&msg);
            let again = JsonCodec
                .decode_server(&body, &Limits::trusted())
                .expect("an encoded server message decodes");
            assert_eq!(again, msg);
        }
    }
});
