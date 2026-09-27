use crate::models::PhotoNumber;

pub fn canonical_number(value: &str) -> Option<String> {
    let digits = value.trim();
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let stripped = digits.trim_start_matches('0');
    Some(if stripped.is_empty() { "0" } else { stripped }.to_string())
}

pub fn extract_number(value: &str) -> Option<PhotoNumber> {
    let original = value.trim().to_string();
    let stem = original
        .rsplit_once('.')
        .map_or(original.as_str(), |(left, _)| left);
    let reversed: String = stem
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let digits: String = reversed.chars().rev().collect();
    Some(PhotoNumber {
        original,
        canonical: canonical_number(&digits)?,
        confidence: None,
        confirmed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{canonical_number, extract_number};

    #[test]
    fn ignores_any_number_of_leading_zeroes() {
        for raw in ["12", "012", "0012"] {
            assert_eq!(canonical_number(raw).as_deref(), Some("12"));
        }
        for raw in ["1234", "01234", "001234"] {
            assert_eq!(canonical_number(raw).as_deref(), Some("1234"));
        }
        assert_eq!(canonical_number("00000").as_deref(), Some("0"));
    }

    #[test]
    fn keeps_non_leading_zeroes() {
        assert_eq!(canonical_number("1002").as_deref(), Some("1002"));
    }

    #[test]
    fn extracts_trailing_digits_before_the_last_extension() {
        let value = extract_number("archive.IMG-0012.JPG").unwrap();
        assert_eq!(value.original, "archive.IMG-0012.JPG");
        assert_eq!(value.canonical, "12");
    }

    #[test]
    fn returns_none_without_trailing_digits() {
        assert_eq!(extract_number("archive.IMG.JPG"), None);
    }

    #[test]
    fn preserves_leading_zeroes_in_original() {
        let value = extract_number(" IMG-01234.JPG ").unwrap();
        assert_eq!(value.original, "IMG-01234.JPG");
        assert_eq!(value.canonical, "1234");
    }
}
