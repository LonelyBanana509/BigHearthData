#![no_main]
//! The JSON parser under both sets of limits: no panic, and a value that parsed writes back to JSON
//! that parses to the same value.

use bhdata_proto::frame::Limits;
use bhdata_proto::json::{parse, to_string};
use bhdata_types::config::GatewayConfig;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let config = GatewayConfig::default();
    for limits in [
        Limits::before_login(&config),
        Limits::after_login(&config),
        Limits::trusted(),
    ] {
        if let Ok(value) = parse(data, &limits) {
            let text = to_string(&value);
            let again = parse(text.as_bytes(), &Limits::trusted()).expect("written JSON parses");
            assert_eq!(again, value);
        }
    }
});
