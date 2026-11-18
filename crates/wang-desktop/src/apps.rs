//! What's inside your apps: Firefox and Chromium/Chrome profiles, VS Code folders and
//! profiles. Read-only discovery, plus the command line that opens exactly the one you name
//! ("Firefox on swe", "Chromium Hung - Work", "VS Code ruby-exercises").

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    /// What the user calls it ("School", "Hung - Work", "ruby-exercises").
    pub name: String,
    /// Extra detail for telling them apart (folder name, account, path).
    pub detail: String,
    /// Command line that opens the app on this profile.
    pub argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AppProfiles {
    pub app: &'static str,
    /// "profile" | "folder"
    pub kind: &'static str,
    pub items: Vec<Profile>,
}

fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_default()
}

/// The first of `cands` that exists on PATH.
fn bin(cands: &[&str]) -> Option<String> {
    let path = std::env::var("PATH").unwrap_or_default();
    let mut dirs: Vec<String> = path.split(':').map(String::from).collect();
    dirs.push("/snap/bin".into());
    cands.iter().find(|c| dirs.iter().any(|d| Path::new(d).join(c).is_file())).map(|c| c.to_string())
}

// ---------- Firefox ----------

/// `profiles.ini` → (name, path relative-or-absolute, is_relative, default).
pub fn parse_profiles_ini(ini: &str) -> Vec<(String, String, bool, bool)> {
    let mut out = vec![];
    let mut cur: Option<(String, String, bool, bool)> = None;
    for line in ini.lines().map(str::trim) {
        if line.starts_with('[') {
            if let Some(c) = cur.take().filter(|c| !c.0.is_empty()) {
                out.push(c);
            }
            if line.starts_with("[Profile") {
                cur = Some((String::new(), String::new(), true, false));
            }
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "Name" => c.0 = v.to_string(),
                "Path" => c.1 = v.to_string(),
                "IsRelative" => c.2 = v != "0",
                "Default" => c.3 = v == "1",
                _ => {}
            }
        }
    }
    if let Some(c) = cur.filter(|c| !c.0.is_empty()) {
        out.push(c);
    }
    out
}

pub fn firefox() -> Option<AppProfiles> {
    let exe = bin(&["firefox"])?;
    for root in [home().join("snap/firefox/common/.mozilla/firefox"), home().join(".mozilla/firefox")] {
        let Ok(ini) = std::fs::read_to_string(root.join("profiles.ini")) else { continue };
        let items = parse_profiles_ini(&ini)
            .into_iter()
            .map(|(name, path, _, default)| Profile {
                detail: format!("{path}{}", if default { ", default" } else { "" }),
                // -P picks the profile; --new-instance so it opens even while another profile runs.
                // GNOME_ACCESSIBILITY: Firefox only shows its controls to wang when asked.
                argv: vec!["env".into(), "GNOME_ACCESSIBILITY=1".into(), exe.clone(), "-P".into(), name.clone(), "--new-instance".into()],
                name,
            })
            .collect::<Vec<_>>();
        if !items.is_empty() {
            return Some(AppProfiles { app: "Firefox", kind: "profile", items });
        }
    }
    None
}

// ---------- Chromium / Chrome / Brave ----------

/// `Local State` → (profile directory, display name, signed-in account if any).
pub fn parse_local_state(json: &str) -> Vec<(String, String, String)> {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return vec![] };
    let mut out: Vec<(String, String, String)> = v["profile"]["info_cache"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(dir, p)| (dir.clone(), p["name"].as_str().unwrap_or(dir).to_string(), p["user_name"].as_str().unwrap_or("").to_string()))
        .collect();
    out.sort_by_key(|p| p.1.to_lowercase());
    out
}

fn chromium_family() -> Vec<AppProfiles> {
    let h = home();
    let fams: [(&'static str, &[&str], Vec<PathBuf>); 3] = [
        ("Chromium", &["chromium", "chromium-browser"], vec![h.join("snap/chromium/common/chromium"), h.join(".config/chromium")]),
        ("Google Chrome", &["google-chrome", "google-chrome-stable"], vec![h.join(".config/google-chrome")]),
        ("Brave", &["brave-browser", "brave"], vec![h.join(".config/BraveSoftware/Brave-Browser"), h.join("snap/brave/current/.config/BraveSoftware/Brave-Browser")]),
    ];
    let mut out = vec![];
    for (app, exes, roots) in fams {
        let Some(exe) = bin(exes) else { continue };
        let Some(text) = roots.iter().find_map(|r| std::fs::read_to_string(r.join("Local State")).ok()) else { continue };
        let items: Vec<Profile> = parse_local_state(&text)
            .into_iter()
            .map(|(dir, name, user)| Profile {
                detail: if user.is_empty() { dir.clone() } else { format!("{dir}, {user}") },
                argv: vec![exe.clone(), format!("--profile-directory={dir}"), "--new-window".into(), "--force-renderer-accessibility".into()],
                name,
            })
            .collect();
        if !items.is_empty() {
            out.push(AppProfiles { app, kind: "profile", items });
        }
    }
    out
}

// ---------- VS Code ----------

/// VS Code's `storage.json` → folders you've opened, and its own profiles.
pub fn parse_vscode_storage(json: &str) -> (Vec<String>, Vec<String>) {
    let Ok(v) = serde_json::from_str::<Value>(json) else { return (vec![], vec![]) };
    let folders = v["profileAssociations"]["workspaces"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(uri, _)| uri.strip_prefix("file://").map(url_decode))
        .collect();
    let profiles = v["userDataProfiles"].as_array().into_iter().flatten().filter_map(|p| p["name"].as_str().map(String::from)).collect();
    (folders, profiles)
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(x);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn vscode() -> Vec<AppProfiles> {
    let Some(exe) = bin(&["code"]) else { return vec![] };
    let Ok(text) = std::fs::read_to_string(home().join(".config/Code/User/globalStorage/storage.json")) else { return vec![] };
    let (folders, profiles) = parse_vscode_storage(&text);
    let mut out = vec![];
    let items: Vec<Profile> = folders
        .into_iter()
        .filter(|f| Path::new(f).exists())
        .map(|f| Profile {
            name: Path::new(&f).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| f.clone()),
            argv: vec![exe.clone(), "--new-window".into(), f.clone()],
            detail: f,
        })
        .collect();
    if !items.is_empty() {
        out.push(AppProfiles { app: "VS Code", kind: "folder", items });
    }
    let items: Vec<Profile> = profiles.into_iter().map(|p| Profile { argv: vec![exe.clone(), "--profile".into(), p.clone(), "--new-window".into()], detail: "VS Code profile".into(), name: p }).collect();
    if !items.is_empty() {
        out.push(AppProfiles { app: "VS Code", kind: "profile", items });
    }
    out
}

/// Everything found on this machine.
pub fn all() -> Vec<AppProfiles> {
    let mut out: Vec<AppProfiles> = firefox().into_iter().collect();
    out.extend(chromium_family());
    out.extend(vscode());
    out
}

fn norm(s: &str) -> String {
    s.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

/// Does `asked` ("firefox", "chrome", "code") mean this app?
pub fn app_matches(app: &str, asked: &str) -> bool {
    let (a, q) = (norm(app), norm(asked));
    !q.is_empty() && (a.contains(&q) || q.contains(&a) || (q == "chrome" && a.contains("chrom")) || (q == "code" && a == "vscode") || (q == "vscode" && a == "vscode"))
}

/// Pick the profile the user named: exact name, then prefix, then substring of name or detail
/// ("work" finds "swe" whose folder is "pxwp9t7q.work").
pub fn pick<'a>(items: &'a [Profile], asked: &str) -> Result<&'a Profile> {
    let q = norm(asked);
    if q.is_empty() {
        bail!("say which one");
    }
    let tiers: [&dyn Fn(&Profile) -> bool; 4] = [
        &|p| norm(&p.name) == q,
        &|p| norm(&p.name).starts_with(&q),
        &|p| norm(&p.name).contains(&q),
        &|p| norm(&p.detail).contains(&q),
    ];
    for t in tiers {
        let hits: Vec<&Profile> = items.iter().filter(|p| t(p)).collect();
        match hits.len() {
            1 => return Ok(hits[0]),
            0 => continue,
            _ => bail!("\"{asked}\" matches several: {}", hits.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")),
        }
    }
    bail!("no \"{asked}\"; the options are: {}", items.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", "))
}

/// Find the app, then the profile/folder within it.
pub fn resolve(app: &str, asked: &str) -> Result<(&'static str, Profile)> {
    let groups: Vec<AppProfiles> = all().into_iter().filter(|g| app_matches(g.app, app)).collect();
    if groups.is_empty() {
        bail!("no profiles found for \"{app}\" (wang knows Firefox, Chromium/Chrome/Brave and VS Code profiles)");
    }
    let items: Vec<Profile> = groups.iter().flat_map(|g| g.items.clone()).collect();
    let p = pick(&items, asked)?.clone();
    Ok((groups[0].app, p))
}

pub fn render(groups: &[AppProfiles]) -> String {
    if groups.is_empty() {
        return "No app profiles found.".into();
    }
    groups
        .iter()
        .map(|g| format!("{} {}s: {}", g.app, g.kind, g.items.iter().map(|p| format!("{} ({})", p.name, p.detail)).collect::<Vec<_>>().join("; ")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "[General]\nStartWithLastProfile=1\n\n[Profile0]\nName=rakuna\nIsRelative=1\nPath=p18Yh0Bc.Profile 1\n\n[Profile2]\nName=swe\nIsRelative=1\nPath=pxwp9t7q.work\n\n[Profile4]\nName=School\nIsRelative=1\nPath=4hlz62n7.School\nDefault=1\n\n[Install4F96D1932A9F858E]\nDefault=4hlz62n7.School\n";

    #[test]
    fn firefox_profiles_ini() {
        let p = parse_profiles_ini(INI);
        assert_eq!(p.len(), 3, "{p:?}");
        assert_eq!(p[2], ("School".into(), "4hlz62n7.School".into(), true, true));
        assert_eq!(p[1].1, "pxwp9t7q.work");
    }

    #[test]
    fn chromium_local_state() {
        let j = r#"{"profile":{"info_cache":{"Profile 4":{"name":"Hung - Work","user_name":"hung@work.com"},"Profile 1":{"name":"Hung"},"Default":{}}}}"#;
        let p = parse_local_state(j);
        assert_eq!(p[0], ("Default".into(), "Default".into(), "".into()));
        assert_eq!(p[1].1, "Hung");
        assert_eq!(p[2], ("Profile 4".into(), "Hung - Work".into(), "hung@work.com".into()));
        assert!(parse_local_state("nope").is_empty());
    }

    #[test]
    fn vscode_storage() {
        let j = r#"{"profileAssociations":{"workspaces":{"file:///home/h/Downloads/The%20Odin%20Project":"__default__","vscode-remote://x":"a"}},"userDataProfiles":[{"name":"Python"}]}"#;
        let (f, p) = parse_vscode_storage(j);
        assert_eq!(f, vec!["/home/h/Downloads/The Odin Project".to_string()]);
        assert_eq!(p, vec!["Python".to_string()]);
    }

    fn prof(name: &str, detail: &str) -> Profile {
        Profile { name: name.into(), detail: detail.into(), argv: vec![] }
    }

    #[test]
    fn picking_what_the_user_said() {
        let items = vec![prof("School", "4hlz62n7.School, default"), prof("swe", "pxwp9t7q.work"), prof("rakuna", "p18Yh0Bc.Profile 1"), prof("b", "mjo94oz4.Default User"), prof("ngochung.rubik2007@gmail.com", "gdqnvbhj")];
        assert_eq!(pick(&items, "school").unwrap().name, "School");
        assert_eq!(pick(&items, "SWE").unwrap().name, "swe");
        assert_eq!(pick(&items, "work").unwrap().name, "swe", "matches the folder name");
        assert_eq!(pick(&items, "rubik").unwrap().name, "ngochung.rubik2007@gmail.com");
        assert_eq!(pick(&items, "b").unwrap().name, "b", "exact beats substring");
        let e = pick(&items, "nope").unwrap_err().to_string();
        assert!(e.contains("School") && e.contains("swe"), "{e}");
        let two = vec![prof("Hung", "Profile 1"), prof("Hung - Work", "Profile 4")];
        assert_eq!(pick(&two, "hung").unwrap().name, "Hung");
        assert!(pick(&two, "profile").unwrap_err().to_string().contains("several"));
        assert!(app_matches("Google Chrome", "chrome") && app_matches("Chromium", "chrome") && app_matches("VS Code", "code"));
        assert!(!app_matches("Firefox", "chrome"));
    }

    #[test]
    fn this_machine() {
        // Whatever is installed here: every entry has a command to open it.
        eprintln!("{}", render(&all()));
        for g in all() {
            assert!(!g.items.is_empty());
            assert!(g.items.iter().all(|p| !p.argv.is_empty() && !p.name.is_empty()), "{g:?}");
        }
    }
}
