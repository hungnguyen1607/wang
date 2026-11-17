//! Wake word: find the assistant's name in a short transcript.

/// If `transcript` addresses `name` ("Hey Jarvis, what's on today?"), the command after the
/// name (possibly empty: "Jarvis?" alone means "I'm listening"). Tolerates one-letter
/// mishearings for names of 5+ letters ("jarvas"), since small speech models slip.
pub fn heard(transcript: &str, name: &str) -> Option<String> {
    let name: Vec<String> = words(name).map(|(w, _)| w).collect();
    if name.is_empty() {
        return None;
    }
    let ws: Vec<(String, usize)> = words(transcript).collect();
    for i in 0..ws.len() {
        if i + name.len() > ws.len() {
            break;
        }
        if ws[i..i + name.len()].iter().zip(&name).all(|((w, _), n)| close(w, n)) {
            let last = &ws[i + name.len() - 1];
            let rest = &transcript[last.1..];
            let rest = rest.trim_start_matches(|c: char| !c.is_alphanumeric());
            return Some(rest.trim_end().to_string());
        }
    }
    None
}

/// Lowercase alphanumeric words, each with the byte offset just past its end.
fn words(s: &str) -> impl Iterator<Item = (String, usize)> + '_ {
    let mut out = vec![];
    let mut cur = String::new();
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() || c == '\'' {
            cur.extend(c.to_lowercase());
        } else if !cur.is_empty() {
            out.push((std::mem::take(&mut cur).replace('\'', ""), i));
        }
    }
    if !cur.is_empty() {
        out.push((cur.replace('\'', ""), s.len()));
    }
    out.into_iter()
}

fn close(heard: &str, want: &str) -> bool {
    heard == want || (want.chars().count() >= 5 && edit_distance(heard, want) <= 1)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_name_and_command() {
        assert_eq!(heard("Hey Jarvis, what's on today?", "Jarvis").as_deref(), Some("what's on today?"));
        assert_eq!(heard(" jarvis.", "Jarvis").as_deref(), Some(""));
        assert_eq!(heard("OK Jarvas turn it down", "jarvis").as_deref(), Some("turn it down"));
        assert_eq!(heard("hey mister wang how are you", "Mister Wang").as_deref(), Some("how are you"));
    }

    #[test]
    fn ignores_everything_else() {
        assert_eq!(heard("what's the weather like", "Jarvis"), None);
        assert_eq!(heard("I want a wing", "wang"), None); // short names must match exactly
        assert_eq!(heard("", "Jarvis"), None);
        assert_eq!(heard("hello", ""), None);
    }
}
