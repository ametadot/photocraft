#![no_main]
use libfuzzer_sys::fuzz_target;
use photocraft_xmp::XmpPacket;
fuzz_target!(|data: &[u8]| {
    let _ = XmpPacket::parse(data);
});
