//! The plaintext STARTTLS negotiation before a certificate is read (SMTP,
//! IMAP): everything the server says, up to the handshake.
//!
//! Input: the first byte picks the protocol, the rest is what the server
//! sends before closing the connection.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((protocol, server)) = data.split_first() else {
        return;
    };
    hora_fuzz::block_on(hora_core::cert::fuzz::negotiate(server, protocol & 1 == 1));
});
