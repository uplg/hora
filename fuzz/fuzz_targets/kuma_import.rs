//! `hora import kuma <file>`: an Uptime Kuma backup, converted to a Hora
//! configuration. Whatever the backup holds (names, URLs, headers written by
//! someone else), what comes out must be TOML, and must go through the
//! loader without bringing it down.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = hora_fuzz::text(data);
    let Ok(converted) = hora_core::import::convert_kuma_to_hora(&text) else {
        return;
    };
    if let Err(err) = toml::from_str::<toml::Table>(&converted) {
        panic!("the converted backup is not TOML: {err}\n{converted}");
    }
    let _ = hora_core::config::parse(&format!("[page]\n[server]\n{converted}"));
});
