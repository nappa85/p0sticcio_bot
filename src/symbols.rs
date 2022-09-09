// this way we avoid unsafe
const fn from_utf8(v: &'static [u8]) -> &'static str {
    match std::str::from_utf8(v) {
        Ok(s) => s,
        Err(_) => unreachable!(),
    }
}

pub static ALERT: &str = from_utf8(&[0xE2, 0x9A, 0xA0]);
pub static GOLF: &str = from_utf8(&[0xE2, 0x9B, 0xB3]);
pub static TRIANGLE: &str = from_utf8(&[0xE2, 0x9B, 0x9B]);
pub static CROSS: &str = from_utf8(&[0xE2, 0xAD, 0x99]);
pub static BRICK: &str = from_utf8(&[0xF0, 0x9F, 0xA7, 0xB1]);
pub static EXPLOSION: &str = from_utf8(&[0xF0, 0x9F, 0x92, 0xA5]);
pub static SCISSORS: &str = from_utf8(&[0xE2, 0x9C, 0x82]);
pub static CHAIN: &str = from_utf8(&[0xF0, 0x9F, 0x94, 0x97]);
pub static UFO: &str = from_utf8(&[0xF0, 0x9F, 0x9B, 0xB8]);
pub static ALARM: &str = from_utf8(&[0xF0, 0x9F, 0x9A, 0xA8]);
pub static FIREWORKS: &str = from_utf8(&[0xF0, 0x9F, 0x8E, 0x86]);
pub static VIRUS: &str = from_utf8(&[0xF0, 0x9F, 0xA6, 0xA0]);
pub static GREEN: &str = from_utf8(&[0xF0, 0x9F, 0x9F, 0xA2]);
pub static BLUE: &str = from_utf8(&[0xF0, 0x9F, 0x94, 0xB5]);

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn invalid() {
        super::from_utf8(&[0xf0, 0x28, 0x8c, 0x28]);
    }
}
