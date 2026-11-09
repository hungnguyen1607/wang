//! Long-term memory: facts in a Markdown file you can read and edit, plus BM25 search.
//!
//! One fact per bullet line in `~/.local/share/wang/memory.md` (hidden from the jail).
//! Relevant facts are attached to the user's message (not the system prompt), so the
//! cached prompt prefix never changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

pub struct Memory {
    path: PathBuf,
    lock: Mutex<()>,
}

const HEADER: &str = "# wang memory\n\nOne fact per line. Edit freely; wang re-reads this file on every search.\n\n";

const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "is", "are", "was", "be", "it", "my", "me", "i", "you",
    "your", "with", "at", "by", "from", "this", "that", "what", "do", "does", "can", "how", "please", "about", "as", "so",
    "user", "users", "wang", "is", "has", "have", "will", "would", "should", "could", "get", "make", "want", "need", "some",
    "any", "all", "there", "here", "when", "where", "which", "who", "why", "if", "then", "than", "not", "no", "yes",
];

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .map(str::to_lowercase)
        .filter(|w| !STOP.contains(&w.as_str()))
        .collect()
}

impl Memory {
    pub fn new(path: PathBuf) -> Self {
        Self { path, lock: Mutex::new(()) }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every fact (bullet lines, without the bullet).
    pub fn facts(&self) -> Vec<String> {
        std::fs::read_to_string(&self.path)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim_start().strip_prefix("- ").map(|f| f.trim().to_string()))
            .filter(|f| !f.is_empty())
            .collect()
    }

    pub fn add(&self, fact: &str, date: &str) -> Result<String> {
        let fact = fact.split_whitespace().collect::<Vec<_>>().join(" ");
        if fact.is_empty() {
            bail!("empty fact");
        }
        if fact.chars().count() > 500 {
            bail!("facts are limited to 500 characters; save the key point");
        }
        let _g = self.lock.lock().unwrap();
        let lower = fact.to_lowercase();
        if self.facts().iter().any(|f| f.to_lowercase().starts_with(&lower)) {
            return Ok(format!("already remembered: {fact}"));
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut s = std::fs::read_to_string(&self.path).unwrap_or_else(|_| HEADER.to_string());
        if !s.ends_with('\n') {
            s.push('\n');
        }
        s += &format!("- {fact} _({date})_\n");
        std::fs::write(&self.path, s).with_context(|| format!("writing {}", self.path.display()))?;
        Ok(format!("remembered: {fact}"))
    }

    /// BM25 over facts. Returns (score, fact), best first.
    pub fn search(&self, query: &str, k: usize) -> Vec<(f64, String)> {
        let facts = self.facts();
        let docs: Vec<Vec<String>> = facts.iter().map(|f| tokens(f)).collect();
        let q = tokens(query);
        if docs.is_empty() || q.is_empty() {
            return vec![];
        }
        let n = docs.len() as f64;
        let avg = docs.iter().map(Vec::len).sum::<usize>() as f64 / n;
        let mut df: HashMap<&str, f64> = HashMap::new();
        for d in &docs {
            let mut seen: Vec<&str> = d.iter().map(String::as_str).collect();
            seen.sort();
            seen.dedup();
            for w in seen {
                *df.entry(w).or_default() += 1.0;
            }
        }
        let (k1, b) = (1.2, 0.75);
        let mut scored: Vec<(f64, String)> = docs
            .iter()
            .zip(&facts)
            .map(|(d, f)| {
                let len = d.len() as f64;
                let score: f64 = q
                    .iter()
                    .map(|w| {
                        let tf = d.iter().filter(|x| *x == w).count() as f64;
                        if tf == 0.0 {
                            return 0.0;
                        }
                        let dfw = df.get(w.as_str()).copied().unwrap_or(0.0);
                        let idf = (1.0 + (n - dfw + 0.5) / (dfw + 0.5)).ln();
                        idf * tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * len / avg.max(1.0)))
                    })
                    .sum();
                (score, f.clone())
            })
            .filter(|(s, _)| *s > 0.0)
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        scored.truncate(k);
        scored
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> (Memory, PathBuf) {
        let p = std::env::temp_dir().join(format!("wang-mem-{}-{:?}.md", std::process::id(), std::thread::current().id()));
        let _ = std::fs::remove_file(&p);
        (Memory::new(p.clone()), p)
    }

    #[test]
    fn add_and_search() {
        let (m, p) = mem();
        m.add("My dentist is Dr. Lee on Queen Street", "2026-11-07").unwrap();
        m.add("I prefer metric units", "2026-11-07").unwrap();
        m.add("The home wifi printer is called brother-hl", "2026-11-07").unwrap();
        assert!(m.add("I prefer metric units", "2026-11-08").unwrap().starts_with("already"));
        assert_eq!(m.facts().len(), 3);
        let hits = m.search("when is my dentist appointment?", 3);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].1.contains("Dr. Lee"));
        assert!(m.search("the and of", 3).is_empty());
        // the file is plain Markdown the user can edit
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.starts_with("# wang memory"));
        assert!(text.contains("- I prefer metric units _(2026-11-07)_"));
        std::fs::write(&p, format!("{text}- hand-added fact about bicycles\n")).unwrap();
        assert!(m.search("bicycles", 1)[0].1.contains("hand-added"));
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn rejects_empty_and_huge() {
        let (m, _) = mem();
        assert!(m.add("   ", "d").is_err());
        assert!(m.add(&"x".repeat(600), "d").is_err());
    }
}
