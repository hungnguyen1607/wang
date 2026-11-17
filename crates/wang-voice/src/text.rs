//! Streamed reply → speakable sentences.

/// Collects streamed text and hands out complete sentences, cleaned up for speech.
#[derive(Default)]
pub struct Sentences {
    buf: String,
    /// Inside a ``` block, which isn't read aloud.
    in_code: bool,
}

/// Words that end with a dot but don't end a sentence.
const ABBREV: &[&str] = &["e.g", "i.e", "etc", "mr", "mrs", "ms", "dr", "vs", "approx", "no", "st"];

impl Sentences {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a streamed chunk; returns the sentences it completed.
    pub fn push(&mut self, delta: &str) -> Vec<String> {
        self.buf.push_str(delta);
        let mut out = vec![];
        while let Some(end) = self.boundary() {
            let s: String = self.buf.drain(..end).collect();
            if let Some(s) = self.keep(&s) {
                out.push(s);
            }
        }
        out
    }

    /// The reply ended: whatever is left.
    pub fn finish(&mut self) -> Option<String> {
        let s = std::mem::take(&mut self.buf);
        self.keep(&s)
    }

    /// Track code fences across chunks; speak only text outside them.
    fn keep(&mut self, s: &str) -> Option<String> {
        if s.trim_start().starts_with("```") {
            self.in_code = !self.in_code;
            return None;
        }
        if self.in_code { None } else { speakable(s) }
    }

    /// Byte index just past the first sentence end, if the buffer holds a complete one.
    fn boundary(&self) -> Option<usize> {
        let b = self.buf.as_bytes();
        for (i, &c) in b.iter().enumerate() {
            if c == b'\n' {
                return Some(i + 1);
            }
            if !matches!(c, b'.' | b'!' | b'?') {
                continue;
            }
            // Need to see what follows: whitespace ends the sentence; end of buffer = wait.
            let Some(&next) = b.get(i + 1) else { return None };
            if !next.is_ascii_whitespace() {
                continue; // 3.5, example.com, "?!"
            }
            if c == b'.' {
                let word = self.buf[..i].rsplit(|ch: char| ch.is_whitespace()).next().unwrap_or("");
                let w = word.trim_start_matches(|ch: char| !ch.is_alphanumeric()).to_lowercase();
                if ABBREV.contains(&w.as_str()) || (w.len() == 1 && w.chars().all(|ch| ch.is_alphabetic())) {
                    continue;
                }
            }
            return Some(i + 1);
        }
        None
    }
}

/// Strip Markdown and symbols that sound bad read aloud; None if nothing is left to say.
pub fn speakable(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    for line in s.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let t = t.trim_start_matches(['#', '>', '-', '*', '•']).trim_start();
        if !out.is_empty() && !t.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    let out: String = out.chars().filter(|c| !matches!(c, '*' | '`' | '_' | '#')).collect();
    let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    out.chars().any(char::is_alphanumeric).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&str]) -> Vec<String> {
        let mut s = Sentences::new();
        let mut out: Vec<String> = chunks.iter().flat_map(|c| s.push(c)).collect();
        out.extend(s.finish());
        out
    }

    #[test]
    fn splits_streamed_sentences() {
        assert_eq!(all(&["Good eve", "ning. Your bu", "ild passed! Want the ", "log?"]), vec!["Good evening.", "Your build passed!", "Want the log?"]);
    }

    #[test]
    fn keeps_decimals_domains_and_abbreviations() {
        assert_eq!(all(&["It costs $3.50 at example.com, e.g. the small one. Done."]), vec!["It costs $3.50 at example.com, e.g. the small one.", "Done."]);
        assert_eq!(all(&["Ask Dr. Smith. Then J. Doe."]), vec!["Ask Dr. Smith.", "Then J. Doe."]);
    }

    #[test]
    fn waits_for_what_follows_a_dot() {
        let mut s = Sentences::new();
        assert!(s.push("Version 3.").is_empty());
        assert_eq!(s.push("5 is out. "), vec!["Version 3.5 is out."]);
    }

    #[test]
    fn strips_markdown_and_code() {
        assert_eq!(speakable("- **Two** files: `a.txt`"), Some("Two files: a.txt".into()));
        assert_eq!(speakable("```\nls -la\n```"), None);
        assert_eq!(all(&["Here:\n", "```\nrm x\n```\n", "Done."]), vec!["Here:", "Done."]);
    }
}
