//! Skills: how-tos wang keeps and reuses. Each is a folder with a `SKILL.md` (frontmatter
//! `name` + `description`, then instructions), the same format Claude and OpenClaw skills use,
//! so you can copy one in.
//!
//! - Every message carries a short index (name: description), so wang knows what it has.
//! - The full instructions load only when wang picks one (`skill` tool, `load`).
//! - wang can write a new skill when you say "remember how you did that" (`save`), always
//!   with your approval, showing the full text.
//!
//! The folder lives under ~/.config/wang, which wang's sandbox hides: the agent can't rewrite
//! skills with shell commands, only through `save`, which asks you.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub dir: PathBuf,
}

pub struct Skills {
    pub dir: PathBuf,
}

/// Lowercase letters, digits and dashes, like Claude's skill names.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !n.starts_with('-')
}

/// Split `---\nkey: value\n---\nbody`. Values may be quoted.
pub fn parse(text: &str) -> Option<(String, String, String)> {
    let t = text.trim_start_matches('\u{feff}');
    let rest = t.strip_prefix("---")?.trim_start_matches(['\r', '\n']);
    let end = rest.find("\n---")?;
    let (head, body) = (&rest[..end], &rest[end + 4..]);
    let body = body.trim_start_matches(['-']).trim_start_matches(['\r', '\n']);
    let field = |k: &str| {
        head.lines().find_map(|l| {
            let (key, v) = l.split_once(':')?;
            (key.trim() == k).then(|| v.trim().trim_matches(|c| c == '"' || c == '\'').to_string())
        })
    };
    Some((field("name")?, field("description").unwrap_or_default(), body.trim().to_string()))
}

impl Skills {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn list(&self) -> Vec<Skill> {
        let mut out: Vec<Skill> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let dir = e.path();
                let text = std::fs::read_to_string(dir.join("SKILL.md")).ok()?;
                let (name, description, body) = parse(&text)?;
                Some(Skill { name, description, body, dir })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn get(&self, name: &str) -> Option<Skill> {
        self.list().into_iter().find(|s| s.name == name)
    }

    /// What rides with each message: one line per skill, capped.
    pub fn index(&self, max_chars: usize) -> String {
        let mut s = String::new();
        for k in self.list() {
            let line = format!("- {}: {}\n", k.name, k.description.chars().take(160).collect::<String>());
            if s.len() + line.len() > max_chars {
                s += "- (more: skill list)\n";
                break;
            }
            s += &line;
        }
        s
    }

    /// The full instructions, plus the other files in its folder (paths only).
    pub fn load(&self, name: &str) -> Result<String> {
        let k = self.get(name).with_context(|| format!("no skill named '{name}' (skill list shows them)"))?;
        let mut out = format!("# skill: {}\n{}\n\n{}", k.name, k.description, k.body);
        let extra: Vec<String> = std::fs::read_dir(&k.dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n != "SKILL.md")
            .collect();
        if !extra.is_empty() {
            out += &format!("\n\nFiles in this skill's folder ({}): {}", k.dir.display(), extra.join(", "));
        }
        Ok(out)
    }

    pub fn render(name: &str, description: &str, body: &str) -> String {
        format!("---\nname: {name}\ndescription: {}\n---\n\n{}\n", description.replace('\n', " ").trim(), body.trim())
    }

    /// Write (or replace) a skill. Atomic.
    pub fn save(&self, name: &str, description: &str, body: &str) -> Result<PathBuf> {
        if !valid_name(name) {
            bail!("skill names are lowercase letters, digits and dashes (e.g. clean-downloads)");
        }
        if description.trim().is_empty() || body.trim().is_empty() {
            bail!("a skill needs a description (when to use it) and instructions");
        }
        let dir = self.dir.join(name);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");
        let tmp = dir.join(".SKILL.md.tmp");
        std::fs::write(&tmp, Self::render(name, description, body))?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn remove(&self, name: &str) -> Result<bool> {
        if !valid_name(name) {
            return Ok(false);
        }
        let dir = self.dir.join(name);
        if !dir.join("SKILL.md").exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(true)
    }

    /// Install the starter skills that aren't there yet (never overwrites yours).
    pub fn install_starters(&self) -> Result<Vec<String>> {
        let mut added = vec![];
        for (name, text) in STARTERS {
            let dir = self.dir.join(name);
            if dir.exists() {
                continue;
            }
            std::fs::create_dir_all(&dir)?;
            std::fs::write(dir.join("SKILL.md"), text)?;
            added.push(name.to_string());
        }
        Ok(added)
    }
}

pub fn skills_dir() -> PathBuf {
    crate::config::config_path().with_file_name("skills")
}

pub const STARTERS: &[(&str, &str)] = &[
    (
        "sign-up",
        "---\nname: sign-up\ndescription: Register the user for something (a hackathon like ConUHacks, an event, a course, a waitlist) or fill in an application form on a website.\n---\n\n\
1. Find the official page (search if needed) and open the registration form in the browser. If it needs an account first, say so and ask whether to create one or sign in (`wang login <site>` once).\n\
2. Read the whole form (view every part). List what it asks for.\n\
3. Fill what you know from memory and from what the user said (name, email, school, program, year...). Collect everything you don't know (t-shirt size, dietary needs, links, written answers) and ask in ONE message, with short options where possible. Never invent personal details.\n\
4. Long answers (\"why do you want to attend?\"): draft them, show the draft, use it once the user agrees or edits it.\n\
5. Files: upload only the file the user named (browser action upload with its path). If they didn't say which, ask (e.g. list ~/Documents/*.pdf).\n\
6. Required checkboxes (code of conduct, data sharing): tick them only if the user agreed; mention what they are.\n\
7. Submit: click the form's submit button. The user gets an approval showing every field; wait for it.\n\
8. CAPTCHA or \"verify you're human\": stop and tell the user; never try to solve or get around it.\n\
9. After: report what happened (confirmation number, next steps). If a confirmation email is expected, offer to check Gmail and click its link (that click also asks).\n",
    ),
    (
        "morning-brief",
        "---\nname: morning-brief\ndescription: A short start-of-day brief (calendar, important unread mail, weather) for the user's phone. Use for \"brief me\", \"what's my day\", or a scheduled morning brief.\n---\n\n\
1. Calendar: in the browser, open https://calendar.google.com/calendar/r/agenda and read today's events (time, title, place). If it asks to sign in, say the user needs `wang login calendar.google.com` and continue.\n\
2. Mail: open https://mail.google.com/mail/feed/atom (the unread inbox as a list). Pick at most 5 that look personal or urgent (people, school, work, money, deliveries); skip newsletters and promos.\n\
3. Weather: web_fetch https://wttr.in/?format=3 (the user's location is in memory if they said it; else the default).\n\
4. Reply in under 12 lines: the day's events first, then the mails (sender — subject, one line each), then weather. No greetings, no markdown tables.\n\
Read only: never reply to, archive or mark anything.\n",
    ),
    (
        "clean-downloads",
        "---\nname: clean-downloads\ndescription: Tidy ~/Downloads: show what's there by type and age, then sort or delete old files only after the user agrees. Use for \"clean my downloads\", \"free up space\".\n---\n\n\
1. Survey without changing anything: `cd ~/Downloads && find . -maxdepth 1 -type f -printf '%TY-%Tm-%Td %s %f\\n' | sort | tail -50`, plus `du -sh ~/Downloads` and counts by extension.\n\
2. Propose a plan in a few lines: e.g. move installers (.deb, .AppImage, .exe, .dmg) older than 30 days to the trash, put PDFs in ~/Documents/Downloads-PDFs, images in ~/Pictures/Downloads. Say how much space it frees.\n\
3. Wait for the user's yes. Then do it with `gio trash` (recoverable), never `rm`, using privileged shell commands (they ask for approval).\n\
4. Report what moved and the space freed.\n",
    ),
    (
        "youtube-music",
        "---\nname: youtube-music\ndescription: Find music or a playlist on YouTube for the user (\"play lo-fi\", \"find me a study playlist\") and send the link.\n---\n\n\
1. In the browser, open https://www.youtube.com/results?search_query=<words>+playlist (or without \"playlist\" for one song).\n\
2. Read the view; prefer results with many views or from the official channel. Skip ads and Shorts.\n\
3. Reply with the top 3: title, channel, length or video count, and the full https://www.youtube.com/... link, so the user can tap it on their phone.\n\
4. If they asked to play it on the laptop, open the link with a privileged shell command `xdg-open <link>` (asks for approval) so it plays on the laptop's own browser.\n",
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> Skills {
        let d = std::env::temp_dir().join(format!("wang-skills-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Skills::new(d)
    }

    #[test]
    fn frontmatter() {
        let (n, d, b) = parse("---\nname: x-y\ndescription: \"Does: things\"\nlicense: MIT\n---\n\nStep 1\nStep 2\n").unwrap();
        assert_eq!((n.as_str(), d.as_str(), b.as_str()), ("x-y", "Does: things", "Step 1\nStep 2"));
        assert!(parse("no frontmatter").is_none());
        assert!(parse("---\ndescription: no name\n---\nbody").is_none());
        for (name, text) in STARTERS {
            let (n, d, b) = parse(text).unwrap();
            assert_eq!(&n, name);
            assert!(!d.is_empty() && b.len() > 100, "{name}");
        }
    }

    #[test]
    fn save_list_load_remove() {
        let s = tmp("crud");
        assert!(s.list().is_empty() && s.index(500).is_empty());
        assert_eq!(s.install_starters().unwrap().len(), STARTERS.len());
        assert!(s.get("sign-up").is_some());
        assert!(s.install_starters().unwrap().is_empty(), "never twice");
        s.save("print-pdf", "Print a PDF on the office printer", "Use lp -d office <file>.").unwrap();
        std::fs::write(s.dir.join("print-pdf/notes.txt"), "x").unwrap();
        let names: Vec<String> = s.list().into_iter().map(|k| k.name).collect();
        assert!(names.contains(&"print-pdf".to_string()) && names.contains(&"morning-brief".to_string()));
        let idx = s.index(2000);
        assert!(idx.contains("- print-pdf: Print a PDF on the office printer"), "{idx}");
        assert!(s.index(60).contains("(more: skill list)"));
        let body = s.load("print-pdf").unwrap();
        assert!(body.contains("lp -d office") && body.contains("notes.txt"), "{body}");
        assert!(s.load("nope").is_err());
        assert!(s.save("Bad Name", "d", "b").is_err());
        assert!(s.save("../escape", "d", "b").is_err());
        assert!(s.save("ok", "", "b").is_err());
        assert!(s.remove("print-pdf").unwrap());
        assert!(!s.remove("print-pdf").unwrap());
        assert!(!s.remove("../..").unwrap());
        std::fs::remove_dir_all(&s.dir).ok();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saving_needs_approval_loading_doesnt() {
        let sk = tmp("tool");
        let ctx = crate::tools::ToolCtx::new(std::env::temp_dir()).with_skills(Some(std::sync::Arc::new(Skills::new(sk.dir.clone()))));
        assert!(crate::tools::definitions_for(&ctx).iter().any(|d| d["name"] == "skill"));
        let save = serde_json::json!({"action": "save", "name": "greet", "description": "Say hi", "instructions": "Reply hi."});
        let why = ctx.approval_reason("skill", &save, "s").await.unwrap();
        assert!(why.contains("greet") && why.contains("Reply hi."), "{why}");
        assert!(ctx.approval_reason("skill", &serde_json::json!({"action": "load", "name": "greet"}), "s").await.is_none());
        let out = crate::tools::run_in("skill", &save, &ctx, "s").await;
        assert!(!out.is_error, "{}", out.text);
        let out = crate::tools::run_in("skill", &serde_json::json!({"action": "load", "name": "greet"}), &ctx, "s").await;
        assert!(out.text.contains("Reply hi."));
        std::fs::remove_dir_all(&sk.dir).ok();
    }

    #[test]
    fn skills_are_hidden_from_the_sandbox() {
        let d = dirs::home_dir().unwrap().join(".config/wang/skills/x/SKILL.md");
        assert!(wang_jail::policy::is_hidden(&d, &[]));
    }
}
