//! Task-shaped workflows: pick how to work from what the task is.
//!
//! One generic loop with every tool treats "what's my disk usage" and "try three fixes for
//! this flaky test" the same way. Each message is now classified, by a free keyword scorer or
//! an explicit `/research` `/code` `/quick` `/general` prefix, into a workflow that sets:
//! - the guidance the model gets with the message (how to approach this kind of task),
//! - which tools may run (the tool *list* never changes, so the prompt cache stays warm;
//!   calls outside the workflow are refused with a pointer to the right ones),
//! - the step limit.
//!
//! The same workflows are exposed to Claude Code / Claude Desktop as MCP prompts.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Workflow {
    /// Find things out on the web; cite sources.
    Research,
    /// Build, install, run or experiment with code: in a throwaway VM, with checkpoints and tries.
    Code,
    /// Small question or local action: few steps, local tools only.
    Quick,
    /// Anything else: the full toolset.
    General,
    /// Multi-part task: a planner splits it into steps run by separate worker agents (M11).
    Plan,
}

pub const ALL: [Workflow; 5] = [Workflow::Research, Workflow::Code, Workflow::Quick, Workflow::General, Workflow::Plan];

impl Workflow {
    pub fn name(self) -> &'static str {
        match self {
            Workflow::Research => "research",
            Workflow::Code => "code",
            Workflow::Quick => "quick",
            Workflow::General => "general",
            Workflow::Plan => "plan",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        ALL.into_iter().find(|w| w.name() == s)
    }

    pub fn summary(self) -> &'static str {
        match self {
            Workflow::Research => "Research a question on the web from several sources and answer with citations",
            Workflow::Code => "Build, install, run or experiment with code in a throwaway VM; try alternatives in parallel and keep what passes",
            Workflow::Quick => "A small question or local action: few steps, local tools only",
            Workflow::General => "Anything else, with every tool",
            Workflow::Plan => "A multi-part task: plan it into steps, run them with separate worker agents (in parallel where possible), check each, write the answer",
        }
    }

    /// Tools this workflow may run (`None` = all).
    pub fn allowed(self, vm_available: bool) -> Option<&'static [&'static str]> {
        match self {
            Workflow::Research => Some(&["web_fetch", "browser", "recall", "remember", "read_file"]),
            Workflow::Code if vm_available => Some(&["vm", "read_file", "shell", "web_fetch", "browser", "recall", "remember"]),
            Workflow::Code => None,
            Workflow::Quick => Some(&["shell", "read_file", "recall", "remember", "web_fetch", "schedule"]),
            Workflow::General | Workflow::Plan => None,
        }
    }

    /// Step limit for this workflow, given the configured one.
    pub fn max_steps(self, configured: u32) -> u32 {
        match self {
            Workflow::Quick => configured.min(6),
            Workflow::Research => configured.min(20),
            Workflow::Code => configured.max(30),
            Workflow::General | Workflow::Plan => configured,
        }
    }

    /// How to approach this kind of task (sent with the user's message).
    pub fn guidance(self, vm_available: bool) -> &'static str {
        match self {
            Workflow::Research => {
                "Research task. Check at least two independent sources; use web_fetch for plain pages and the browser for pages that need JavaScript or clicking. \
                 Prefer primary sources (official docs, papers, the project's own site). Note dates: say when information may be outdated. \
                 End with a direct answer, then the sources as URLs."
            }
            Workflow::Code if vm_available => {
                "Code task. Do the work in the vm tool, not on the user's machine: create the files there, install what's needed, run it. \
                 Checkpoint before risky steps. When several fixes or approaches are plausible, use vm try with a score command (usually the tests) \
                 and keep the one that passes. Read the user's files with read_file or read-only shell if you need them. \
                 Finish by saying what worked, with the commands and the final code."
            }
            Workflow::Code => {
                "Code task. Work step by step: make the smallest change, run it or its tests, read the error, fix. \
                 Finish by saying what worked, with the commands and the final code."
            }
            Workflow::Quick => "Quick task. Answer in as few steps as possible, with local tools only. Keep the reply short.",
            Workflow::General | Workflow::Plan => "",
        }
    }
}

/// Words that point at each workflow, with weights.
const RESEARCH: &[(&str, i32)] = &[
    ("research", 3), ("look up", 3), ("find out", 3), ("search", 2), ("compare", 2), ("latest", 2), ("news", 2),
    ("sources", 3), ("cite", 3), ("what is the best", 2), ("which is better", 2), ("reviews", 2), ("price of", 2),
    ("documentation", 1), ("docs for", 2), ("how does", 1), ("who is", 2), ("when did", 2), ("summarize this page", 3),
    ("http://", 1), ("https://", 1),
];
const CODE: &[(&str, i32)] = &[
    ("```", 3), ("install", 2), ("pip ", 2), ("npm ", 2), ("cargo ", 2), ("compile", 2), ("build", 1), ("run this", 3),
    ("script", 2), ("benchmark", 2), ("experiment", 2), ("try ", 1), ("fix the", 2), ("failing test", 3), ("debug", 2),
    ("traceback", 3), ("error:", 2), ("write a program", 3), ("write code", 3), ("implement", 2), ("refactor", 2),
    ("python", 1), ("function", 1), ("unit test", 2), ("docker", 1), (".py", 2), (".rs", 2), (".js", 2),
];
const QUICK: &[(&str, i32)] = &[
    ("how much disk", 3), ("disk space", 3), ("what time", 3), ("list files", 3), ("how many files", 3), ("which version", 2),
    ("is running", 2), ("my ip", 3), ("uptime", 3), ("free memory", 3), ("show me", 1), ("rename", 1), ("what's in", 2),
];

fn score(text: &str, words: &[(&str, i32)]) -> i32 {
    words.iter().filter(|(w, _)| text.contains(w)).map(|(_, s)| s).sum()
}

/// Several kinds of work in sequence ("research X, then benchmark Y"): worth planning.
/// Conservative: a planner adds model calls, so single-kind tasks keep the single loop.
fn needs_plan(t: &str, research: i32, code: i32) -> bool {
    if t.split_whitespace().count() < 10 {
        return false;
    }
    let sequencing = [" then ", "after that", "and finally", "afterwards", "step 1", "first,", "once that", "next,"]
        .iter()
        .filter(|w| t.contains(*w))
        .count();
    let numbered = t.lines().filter(|l| {
        let l = l.trim_start();
        l.len() > 2 && l.as_bytes()[0].is_ascii_digit() && matches!(l.as_bytes()[1], b'.' | b')')
    });
    let mixed = research >= 2 && code >= 2;
    sequencing >= 1 && (mixed || sequencing >= 2) || numbered.count() >= 3 || (mixed && research + code >= 7)
}

/// Classify a message. Returns the workflow and the text with any `/workflow` prefix removed.
pub fn classify(text: &str) -> (Workflow, String) {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed.strip_prefix('/') {
        let (word, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        if let Some(w) = Workflow::from_name(&word.to_ascii_lowercase()) {
            return (w, body.trim().to_string());
        }
    }
    let t = text.to_lowercase();
    // "every morning …", "remind me …": something to schedule. The general workflow has the
    // schedule tool (the task itself gets its own workflow when it runs).
    if ["remind me", "every morning", "every evening", "every day", "every week", "every monday", "every hour",
        "each morning", "schedule ", "tomorrow at", "daily at", "every weekday"]
        .iter()
        .any(|w| t.contains(w))
    {
        return (Workflow::General, text.to_string());
    }
    let (r, c, q) = (score(&t, RESEARCH), score(&t, CODE), score(&t, QUICK));
    if needs_plan(&t, r, c) {
        return (Workflow::Plan, text.to_string());
    }
    let best = r.max(c).max(q);
    let w = if best < 2 {
        // Nothing clear: short messages are quick, the rest general.
        if t.split_whitespace().count() <= 8 && !t.contains('?') { Workflow::Quick } else { Workflow::General }
    } else if c == best {
        Workflow::Code
    } else if r == best {
        Workflow::Research
    } else {
        Workflow::Quick
    };
    (w, text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_typical_messages() {
        let cases = [
            ("research the best laptop for rust development under $1500 and cite sources", Workflow::Research),
            ("what's the latest news on the EU AI act?", Workflow::Research),
            ("compare tokio and async-std, which is better?", Workflow::Research),
            ("install pandas and run this script to benchmark csv parsing", Workflow::Code),
            ("fix the failing test in test_checkout.py, here's the traceback: ...", Workflow::Code),
            ("```python\nprint(1/0)\n```\nwhy does this crash", Workflow::Code),
            ("how much disk space do I have left", Workflow::Quick),
            ("what's my uptime", Workflow::Quick),
            ("hi", Workflow::Quick),
            ("every morning at 8 send me the top 5 hacker news posts", Workflow::General),
            ("remind me tomorrow at 9am to call the dentist", Workflow::General),
            ("can you help me plan my week? I have three deadlines and a trip on friday", Workflow::General),
            ("research the three most popular rust http clients, then install the top two and benchmark them on a simple GET", Workflow::Plan),
            ("1. find the latest python release notes\n2. summarize the changes\n3. test the new syntax in a script", Workflow::Plan),
            ("first, look up the docs for serde. then write a small example. after that run it", Workflow::Plan),
        ];
        for (msg, want) in cases {
            assert_eq!(classify(msg).0, want, "{msg}");
        }
    }

    #[test]
    fn explicit_prefix_wins_and_is_stripped() {
        assert_eq!(classify("/research rust async runtimes"), (Workflow::Research, "rust async runtimes".to_string()));
        assert_eq!(classify("/code how much disk space").0, Workflow::Code);
        assert_eq!(classify("/unknown thing").0, Workflow::Quick); // not a workflow: classified normally
        assert_eq!(classify("/plan compare A and B").0, Workflow::Plan);
    }

    #[test]
    fn limits_and_tools() {
        assert_eq!(Workflow::Quick.max_steps(25), 6);
        assert_eq!(Workflow::Code.max_steps(25), 30);
        assert!(Workflow::Research.allowed(true).unwrap().contains(&"browser"));
        assert!(!Workflow::Research.allowed(true).unwrap().contains(&"shell"));
        assert!(Workflow::Code.allowed(true).unwrap().contains(&"vm"));
        assert!(Workflow::Code.allowed(false).is_none());
        assert!(Workflow::General.allowed(true).is_none());
        assert!(Workflow::Code.guidance(true).contains("vm try"));
    }
}
