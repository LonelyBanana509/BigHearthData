#![no_main]
//! The SCRAM message parsers, both directions: any text is a message or an error, never a panic.

use bhdata_proto::scram::{Client, ClientFinal, ClientFirst};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let _ = ClientFirst::parse(text);
    let _ = ClientFinal::parse(text);
    // The client side: `respond` reads the server's first message, `verify` its last.
    if let Ok((state, _)) = Client::start("user", b"pencil", "rOprNGfwEbeRWgbNEkqO".to_owned()) {
        if let Ok((state, _)) = state.respond(text) {
            let _ = state.verify(text);
        }
    }
});
