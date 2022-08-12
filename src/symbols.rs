// this way we avoid unsafe
const fn from_utf8(v: &'static [u8]) -> &'static str {
    match std::str::from_utf8(v) {
        Ok(s) => s,
        Err(_) => unreachable!(),
    }
}

pub static ALERT: &'static str = from_utf8(&[0xE2, 0x9A, 0xA0]);
pub static GOLF: &'static str = from_utf8(&[0xE2, 0x9B, 0xB3]);
pub static TRIANGLE: &'static str = from_utf8(&[0xE2, 0x9B, 0x9B]);
pub static CROSS: &'static str = from_utf8(&[0xE2, 0xAD, 0x99]);
pub static BRICK: &'static str = from_utf8(&[0xF0, 0x9F, 0xA7, 0xB1]);
pub static EXPLOSION: &'static str = from_utf8(&[0xF0, 0x9F, 0x92, 0xA5]);
pub static SCISSORS: &'static str = from_utf8(&[0xE2, 0x9C, 0x82]);
pub static CHAIN: &'static str = from_utf8(&[0xF0, 0x9F, 0x94, 0x97]);
pub static UFO: &'static str = from_utf8(&[0xF0, 0x9F, 0x9B, 0xB8]);
pub static ALARM: &'static str = from_utf8(&[0xF0, 0x9F, 0x9A, 0xA8]);
pub static FIREWORKS: &'static str = from_utf8(&[0xF0, 0x9F, 0x8E, 0x86]);
pub static VIRUS: &'static str = from_utf8(&[0xF0, 0x9F, 0xA6, 0xA0]);
pub static GREEN: &'static str = from_utf8(&[0xF0, 0x9F, 0x9F, 0xA2]);
pub static BLUE: &'static str = from_utf8(&[0xF0, 0x9F, 0x94, 0xB5]);

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn invalid() {
        super::from_utf8(&[0xf0, 0x28, 0x8c, 0x28]);
    }
}
