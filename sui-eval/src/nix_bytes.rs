use std::borrow::Cow;

const ESCAPE_BASE: u32 = 0x0010_FF00;

fn escape_of(c: char) -> Option<u8> {
    let u = c as u32;
    if (ESCAPE_BASE + 0x80..=ESCAPE_BASE + 0xFF).contains(&u) {
        Some((u - ESCAPE_BASE) as u8)
    } else {
        None
    }
}

#[must_use]
pub fn has_escapes(s: &str) -> bool {
    let b = s.as_bytes();
    b.contains(&0xF4)
        && b.windows(3)
        .any(|w| w[0] == 0xF4 && w[1] == 0x8F && (w[2] == 0xBE || w[2] == 0xBF))
}

#[must_use]
pub fn decode(s: &str) -> Cow<'_, [u8]> {
    if !has_escapes(s) {
        return Cow::Borrowed(s.as_bytes());
    }
    let mut out = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    for c in s.chars() {
        match escape_of(c) {
            Some(b) => out.push(b),
            None => out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes()),
        }
    }
    Cow::Owned(out)
}

#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for &b in chunk.invalid() {
            out.push(char::from_u32(ESCAPE_BASE + u32::from(b)).unwrap_or(char::REPLACEMENT_CHARACTER));
        }
    }
    out
}

#[must_use]
pub fn canonical(s: &str) -> Option<String> {
    if has_escapes(s) { Some(encode(&decode(s))) } else { None }
}

#[must_use]
pub fn byte_len(s: &str) -> usize {
    decode(s).len()
}

#[must_use]
pub fn byte_slice(s: &str, start: usize, end: usize) -> Option<String> {
    let bytes = decode(s);
    let end = end.min(bytes.len());
    let start = start.min(end);
    let piece = &bytes[start..end];
    if matches!(bytes, Cow::Borrowed(_)) && s.is_char_boundary(start) && s.is_char_boundary(end) {
        return None;
    }
    Some(encode(piece))
}

#[must_use]
pub fn cmp(a: &str, b: &str) -> std::cmp::Ordering {
    if has_escapes(a) || has_escapes(b) {
        decode(a).cmp(&decode(b))
    } else {
        a.cmp(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_character_round_trips_through_its_bytes() {
        let star = "★";
        let head = byte_slice(star, 0, 1).unwrap();
        let tail = byte_slice(star, 1, 3).unwrap();
        assert_eq!(byte_len(&head), 1);
        assert_eq!(byte_len(&tail), 2);
        assert_ne!(head, "/");
        assert_eq!(canonical(&format!("{head}{tail}")).as_deref(), Some(star));
    }

    #[test]
    fn a_whole_character_slice_needs_no_escape() {
        assert_eq!(byte_slice("a★b", 1, 4), None);
        assert_eq!(canonical("plain"), None);
    }

    #[test]
    fn order_is_byte_order() {
        let head = byte_slice("★", 0, 1).unwrap();
        assert_eq!(cmp(&head, "★"), std::cmp::Ordering::Less);
        assert_eq!(cmp(&head, "\u{7f}"), std::cmp::Ordering::Greater);
    }
}
