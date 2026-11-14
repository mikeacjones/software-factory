//! Formatted and with a passing test, with one clippy warning on purpose:
//! `cargo xtask check` must fail here, in the lint stage only.

pub fn double(n: u32) -> u32 {
    return n * 2;
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubles() {
        assert_eq!(super::double(2), 4);
    }
}
