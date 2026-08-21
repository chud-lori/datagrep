#![deny(missing_debug_implementations)]
#![warn(rust_2018_idioms)]

mod error;
mod reference;
mod resolver;

pub use error::SecretError;
pub use reference::SecretRef;
pub use resolver::SecretResolver;

pub(crate) fn wipe(s: &mut str) {
    // SAFETY: 0x00 is a one-byte UTF-8 scalar, so the String invariants hold.
    let bytes = unsafe { s.as_bytes_mut() };
    for b in bytes.iter_mut() {
        // SAFETY: `b` is a valid, aligned, exclusive reference into the buffer.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::wipe;

    #[test]
    fn wipe_zeroes_bytes() {
        let mut s = String::from("swordfish");
        wipe(&mut s);
        assert!(s.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(s.len(), 9);
    }
}
