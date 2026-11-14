//! Formatted and lint-clean, with one test that fails on purpose:
//! `cargo xtask check` must fail here, in the test stage only.

pub fn double(n: u32) -> u32 {
    n * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn fails_on_purpose() {
        assert_eq!(super::double(2), 5);
    }
}
