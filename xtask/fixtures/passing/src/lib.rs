//! Clean in every stage: `cargo xtask check` must pass here.

pub fn double(n: u32) -> u32 {
    n * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubles() {
        assert_eq!(super::double(2), 4);
    }
}
