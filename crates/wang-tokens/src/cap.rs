/// Result of capping a tool output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capped {
    pub text: String,
    pub raw_bytes: usize,
    pub sent_bytes: usize,
}

impl Capped {
    pub fn truncated(&self) -> bool {
        self.sent_bytes < self.raw_bytes
    }
}

/// Keep the first ~2/3 and last ~1/3 of `s` (by chars) when it exceeds `max_chars`.
/// Errors and summaries tend to live at the end of outputs, so the tail matters.
pub fn cap_middle(s: &str, max_chars: usize) -> Capped {
    let raw_bytes = s.len();
    let total = s.chars().count();
    if total <= max_chars {
        return Capped { text: s.to_string(), raw_bytes, sent_bytes: raw_bytes };
    }
    let head_n = max_chars * 2 / 3;
    let tail_n = max_chars - head_n;
    let head_end = s.char_indices().nth(head_n).map_or(s.len(), |(i, _)| i);
    let tail_start = s.char_indices().nth(total - tail_n).map_or(s.len(), |(i, _)| i);
    let omitted = total - head_n - tail_n;
    let text = format!(
        "{}\n\n[... {omitted} chars omitted by wang; narrow the request (offset/limit, grep, head/tail) to see more ...]\n\n{}",
        &s[..head_end],
        &s[tail_start..]
    );
    let sent_bytes = text.len();
    Capped { text, raw_bytes, sent_bytes }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_untouched() {
        let c = cap_middle("hello", 10);
        assert_eq!(c.text, "hello");
        assert!(!c.truncated());
    }

    #[test]
    fn long_keeps_head_and_tail() {
        let s: String = (0..1000).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
        let c = cap_middle(&s, 90);
        assert!(c.truncated());
        assert!(c.text.starts_with(&s[..60]));
        assert!(c.text.ends_with(&s[970..]));
        assert!(c.text.contains("910 chars omitted"));
    }

    #[test]
    fn multibyte_safe() {
        let s = "é".repeat(500);
        let c = cap_middle(&s, 30);
        assert!(c.text.starts_with(&"é".repeat(20)));
    }
}
