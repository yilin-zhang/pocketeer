//! How values are written for people, the same in the command line and the
//! terminal UI.

/// A number of bytes written the way people read sizes.
pub trait ByteCountExt {
    /// Whole bytes below 1 KiB, and otherwise the largest unit that keeps the
    /// number at 1 or more, with one decimal unless it is whole.
    fn readable_bytes(&self) -> String;
}

impl ByteCountExt for u64 {
    fn readable_bytes(&self) -> String {
        if *self < 1024 {
            return format!("{self} bytes");
        }
        let units = ["KiB", "MiB", "GiB", "TiB"];
        let mut amount = *self as f64 / 1024.0;
        let mut unit_index = 0;
        while amount >= 1024.0 && unit_index + 1 < units.len() {
            amount /= 1024.0;
            unit_index += 1;
        }
        let rounded = format!("{amount:.1}");
        let rounded = rounded.strip_suffix(".0").unwrap_or(&rounded);
        format!("{rounded} {}", units[unit_index])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_bytes_in_the_largest_unit_with_one_decimal() {
        assert_eq!(0_u64.readable_bytes(), "0 bytes");
        assert_eq!(1023_u64.readable_bytes(), "1023 bytes");
        assert_eq!(1024_u64.readable_bytes(), "1 KiB");
        assert_eq!(32768_u64.readable_bytes(), "32 KiB");
        assert_eq!(1572864_u64.readable_bytes(), "1.5 MiB");
        assert_eq!(1234567_u64.readable_bytes(), "1.2 MiB");
    }
}
