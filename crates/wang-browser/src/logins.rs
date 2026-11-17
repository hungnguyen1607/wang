//! Logins for wang's browser, from one of two places:
//!
//! - **wang's own login file** (`LoginSource::Jar`): you sign in once in a visible window
//!   (`wang browser login`), the cookies are saved to a private file under
//!   `~/.local/share/wang/` (hidden from every jail), loaded into each new headless browser, and
//!   saved back when it closes so refreshed sessions persist.
//! - **Firefox** (`LoginSource::Firefox`): a copy of the cookie database is read (never the live
//!   file); Firefox keeps working as normal.
//!
//! The daemon does the reading and writing, so the jailed browser never sees either file. Only
//! cookies for the allowed sites are used.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq)]
pub enum LoginSource {
    /// wang's own login file (JSON list of CDP cookies, 0600).
    Jar(PathBuf),
    /// A Firefox profile directory (read-only, copied).
    Firefox(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoginsCfg {
    pub source: LoginSource,
    /// Sites whose logins are used: "instagram.com" also covers its subdomains; "*" = every site.
    pub sites: Vec<String>,
}

impl LoginsCfg {
    /// Cookies for the allowed sites, as CDP `Storage.setCookies` entries.
    pub fn load(&self) -> Result<Vec<Value>> {
        match &self.source {
            LoginSource::Jar(path) => read_jar(path, &self.sites, now_secs()),
            LoginSource::Firefox(profile) => read_cookies(profile, &self.sites),
        }
    }
}

/// Only the fields `Storage.setCookies` takes. Session cookies (no expiry) stay session cookies.
/// Partitioned cookies are dropped: they belong to one embedding site, not to a login.
pub fn to_param(c: &Value) -> Option<Value> {
    if c.get("partitionKey").is_some_and(|k| !k.is_null()) {
        return None;
    }
    let mut out = json!({});
    for k in ["name", "value", "domain", "path", "secure", "httpOnly", "sameSite", "priority", "sourceScheme", "sourcePort"] {
        if let Some(v) = c.get(k).filter(|v| !v.is_null()) {
            out[k] = v.clone();
        }
    }
    let session = c.get("session").and_then(Value::as_bool).unwrap_or(false);
    if let Some(e) = c.get("expires").and_then(Value::as_f64).filter(|e| !session && *e > 0.0) {
        out["expires"] = json!(e);
    }
    (out.get("name").is_some() && out.get("domain").is_some()).then_some(out)
}

fn domain_of(c: &Value) -> String {
    c["domain"].as_str().unwrap_or("").trim_start_matches('.').to_ascii_lowercase()
}

pub(crate) fn read_jar(path: &Path, sites: &[String], now: f64) -> Result<Vec<Value>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let all: Vec<Value> = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(all
        .iter()
        .filter(|c| site_matches(&domain_of(c), sites))
        .filter(|c| c.get("expires").and_then(Value::as_f64).is_none_or(|e| e > now))
        .filter_map(to_param)
        .collect())
}

/// Save `cookies` (from `Storage.getCookies`) into the jar, for the allowed sites.
/// - Sites in `replace`: the browser's cookies replace the jar's, so logouts stick too.
/// - Other sites: added only if the jar has nothing for them yet, so a browser that merely
///   visited a site can't clobber a login made elsewhere.
pub fn save_jar(path: &Path, cookies: &[Value], sites: &[String], replace: &[String]) -> Result<usize> {
    let norm = |h: &str| h.trim_start_matches('.').to_ascii_lowercase();
    let replace: std::collections::HashSet<String> = replace.iter().map(|h| norm(h)).collect();
    let mut jar: Vec<Value> = match std::fs::read_to_string(path) {
        Ok(t) => serde_json::from_str(&t).unwrap_or_default(),
        Err(_) => vec![],
    };
    let in_jar: std::collections::HashSet<String> = jar.iter().map(domain_of).collect();
    let fresh: Vec<Value> = cookies
        .iter()
        .filter(|c| site_matches(&domain_of(c), sites))
        .filter(|c| replace.contains(&domain_of(c)) || !in_jar.contains(&domain_of(c)))
        .filter_map(to_param)
        .collect();
    jar.retain(|c| !replace.contains(&domain_of(c)));
    jar.extend(fresh);
    write_private(path, &serde_json::to_vec_pretty(&jar)?)?;
    Ok(jar.len())
}

/// Write a file only the user can read, atomically.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Firefox profile roots: snap first (Ubuntu's default), then the classic location.
pub fn firefox_roots() -> Vec<PathBuf> {
    let home = dirs::home_dir().unwrap_or_default();
    vec![home.join("snap/firefox/common/.mozilla/firefox"), home.join(".mozilla/firefox")]
}

/// Find a Firefox profile by its name in profiles.ini (case-insensitive), or the default one
/// when `name` is empty.
pub fn find_firefox_profile(name: &str) -> Result<PathBuf> {
    for root in firefox_roots() {
        let Ok(ini) = std::fs::read_to_string(root.join("profiles.ini")) else { continue };
        if let Some(p) = pick_profile(&ini, name) {
            let dir = if p.1 { root.join(&p.0) } else { PathBuf::from(&p.0) };
            if dir.join("cookies.sqlite").is_file() {
                return Ok(dir);
            }
            bail!("Firefox profile {} has no cookies.sqlite", dir.display());
        }
    }
    if name.is_empty() { bail!("no default Firefox profile found") } else { bail!("no Firefox profile named {name:?} in profiles.ini") }
}

/// (path, is_relative) of the profile named `name`, or the default profile. The `[Install…]`
/// section's `Default=` (what Firefox actually opens) wins over a `[Profile…]` `Default=1`.
fn pick_profile(ini: &str, name: &str) -> Option<(String, bool)> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = vec![];
    for line in ini.lines().map(str::trim) {
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            sections.push((s.to_string(), vec![]));
        } else if let (Some((k, v)), Some(last)) = (line.split_once('='), sections.last_mut()) {
            last.1.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let get = |kv: &[(String, String)], k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, b)| b.clone());
    let profiles: Vec<&Vec<(String, String)>> = sections.iter().filter(|(s, _)| s.starts_with("Profile")).map(|(_, kv)| kv).collect();
    let entry = |kv: &[(String, String)]| get(kv, "Path").map(|p| (p, get(kv, "IsRelative").as_deref() != Some("0")));
    if !name.is_empty() {
        return profiles.iter().find(|kv| get(kv, "Name").is_some_and(|n| n.eq_ignore_ascii_case(name))).and_then(|kv| entry(kv));
    }
    if let Some(path) = sections.iter().filter(|(s, _)| s.starts_with("Install")).find_map(|(_, kv)| get(kv, "Default")) {
        let rel = profiles.iter().find(|kv| get(kv, "Path").as_deref() == Some(path.as_str())).map(|kv| get(kv, "IsRelative").as_deref() != Some("0"));
        return Some((path, rel.unwrap_or(true)));
    }
    profiles.iter().find(|kv| get(kv, "Default").as_deref() == Some("1")).and_then(|kv| entry(kv)).or_else(|| profiles.first().and_then(|kv| entry(kv)))
}

/// Does `host` belong to one of `sites`? ("youtube.com" covers "www.youtube.com", "*" covers all.)
pub fn site_matches(host: &str, sites: &[String]) -> bool {
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    sites.iter().any(|s| {
        let s = s.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase();
        s == "*" || host == s || host.ends_with(&format!(".{s}"))
    })
}

/// Firefox cookies for the allowed sites, as CDP `Storage.setCookies` entries.
pub fn read_cookies(profile: &Path, sites: &[String]) -> Result<Vec<Value>> {
    // Copy the database (and its write-ahead log, which holds recent logins) so a running
    // Firefox's lock doesn't matter and the live file is never opened.
    let tmp = std::env::temp_dir().join(format!("wang-cookies-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&tmp)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))?;
    }
    let result = (|| {
        let db = tmp.join("cookies.sqlite");
        std::fs::copy(profile.join("cookies.sqlite"), &db).with_context(|| format!("copying cookies from {}", profile.display()))?;
        let wal = profile.join("cookies.sqlite-wal");
        if wal.is_file() {
            std::fs::copy(&wal, tmp.join("cookies.sqlite-wal")).ok();
        }
        cookies_from_db(&db, sites, now_secs())
    })();
    std::fs::remove_dir_all(&tmp).ok();
    result
}

fn rand_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
    h.finish()
}

fn now_secs() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

pub(crate) fn cookies_from_db(db: &Path, sites: &[String], now: f64) -> Result<Vec<Value>> {
    let conn = rusqlite::Connection::open(db)?;
    let mut st = conn.prepare(
        "SELECT name, value, host, path, expiry, isSecure, isHttpOnly, sameSite FROM moz_cookies WHERE originAttributes = ''",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
        ))
    })?;
    let mut out = vec![];
    for row in rows {
        let (name, value, host, path, expiry, secure, http_only, same_site) = row?;
        if !site_matches(&host, sites) {
            continue;
        }
        // Newer Firefox stores expiry in milliseconds, older in seconds.
        let expires = if expiry > 100_000_000_000 { expiry as f64 / 1000.0 } else { expiry as f64 };
        if expires <= now {
            continue;
        }
        let mut c = json!({"name": name, "value": value, "domain": host, "path": path, "expires": expires,
                           "secure": secure != 0, "httpOnly": http_only != 0});
        match same_site {
            1 => c["sameSite"] = json!("Lax"),
            2 => c["sameSite"] = json!("Strict"),
            // SameSite=None is only valid on secure cookies.
            0 if secure != 0 => c["sameSite"] = json!("None"),
            _ => {}
        }
        out.push(c);
    }
    Ok(out)
}

/// Distinct sites (cookie hosts without a leading dot) in a cookie list: where wang is signed in.
pub fn hosts_of(cookies: &[Value]) -> Vec<String> {
    let mut h: Vec<String> = cookies.iter().filter_map(|c| c["domain"].as_str()).map(|d| d.trim_start_matches('.').to_ascii_lowercase()).collect();
    h.sort();
    h.dedup();
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const INI: &str = "[Install4F96D1932A9F858E]\nDefault=pxwp9t7q.work\nLocked=1\n\n[Profile1]\nName=b\nIsRelative=1\nPath=mjo94oz4.Default User\n\n\
[Profile2]\nName=swe\nIsRelative=1\nPath=pxwp9t7q.work\n\n[Profile4]\nName=School\nIsRelative=1\nPath=4hlz62n7.School\nDefault=1\n\n[Profile5]\nName=abs\nIsRelative=0\nPath=/data/ff\n";

    #[test]
    fn picks_profiles() {
        assert_eq!(pick_profile(INI, ""), Some(("pxwp9t7q.work".into(), true)), "Install section wins");
        assert_eq!(pick_profile(INI, "school"), Some(("4hlz62n7.School".into(), true)));
        assert_eq!(pick_profile(INI, "abs"), Some(("/data/ff".into(), false)));
        assert_eq!(pick_profile(INI, "nope"), None);
        let no_install = "[Profile0]\nName=a\nPath=a.x\n\n[Profile1]\nName=b\nPath=b.x\nDefault=1\n";
        assert_eq!(pick_profile(no_install, ""), Some(("b.x".into(), true)));
    }

    #[test]
    fn matches_sites() {
        let s = vec!["instagram.com".to_string(), "*.youtube.com".to_string()];
        assert!(site_matches(".instagram.com", &s) && site_matches("www.instagram.com", &s) && site_matches("m.youtube.com", &s));
        assert!(!site_matches("evilinstagram.com", &s) && !site_matches("google.com", &s));
        assert!(site_matches("anything.org", &["*".to_string()]));
        assert!(!site_matches("x.com", &[]));
    }

    #[test]
    fn reads_only_allowed_live_default_container_cookies() {
        let dir = std::env::temp_dir().join(format!("wang-ff-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("cookies.sqlite");
        let _ = std::fs::remove_file(&db);
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch(
            "CREATE TABLE moz_cookies (id INTEGER PRIMARY KEY, originAttributes TEXT NOT NULL DEFAULT '', name TEXT, value TEXT, host TEXT, path TEXT,
             expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER, isSecure INTEGER, isHttpOnly INTEGER, inBrowserElement INTEGER DEFAULT 0,
             sameSite INTEGER DEFAULT 0, rawSameSite INTEGER DEFAULT 0, schemeMap INTEGER DEFAULT 0);
             INSERT INTO moz_cookies (originAttributes, name, value, host, path, expiry, isSecure, isHttpOnly, sameSite) VALUES
               ('', 'sessionid', 's1', '.instagram.com', '/', 2000000000, 1, 1, 1),
               ('', 'ms', 'v', '.youtube.com', '/', 2000000000000, 1, 0, 0),
               ('', 'old', 'x', '.instagram.com', '/', 1000, 1, 0, 0),
               ('^userContextId=2', 'work', 'w', '.instagram.com', '/', 2000000000, 1, 0, 0),
               ('', 'bank', 'b', '.bank.com', '/', 2000000000, 1, 1, 2),
               ('', 'lax', 'l', 'www.instagram.com', '/', 2000000000, 0, 0, 0);",
        )
        .unwrap();
        drop(c);
        let sites = vec!["instagram.com".to_string(), "youtube.com".to_string()];
        let got = cookies_from_db(&db, &sites, 1_800_000_000.0).unwrap();
        let names: Vec<&str> = got.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["sessionid", "ms", "lax"], "no expired, container or other-site cookies");
        assert_eq!(got[0]["sameSite"], "Lax");
        assert_eq!(got[0]["httpOnly"], true);
        assert_eq!(got[1]["expires"], 2_000_000_000.0, "millisecond expiry converted");
        assert_eq!(got[1]["sameSite"], "None");
        assert!(got[2].get("sameSite").is_none(), "insecure SameSite=None cookie keeps the browser default");
        assert_eq!(hosts_of(&got), vec!["instagram.com", "www.instagram.com", "youtube.com"]);

        let all = cookies_from_db(&db, &["*".to_string()], 1_800_000_000.0).unwrap();
        assert_eq!(all.len(), 4);

        let cfg = LoginsCfg { source: LoginSource::Firefox(dir.clone()), sites };
        assert_eq!(cfg.load().unwrap().len(), 3, "the copy-then-read path gives the same cookies");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn jar_round_trip_replaces_per_site_and_keeps_others() {
        let dir = std::env::temp_dir().join(format!("wang-jar-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jar = dir.join("logins.json");
        let all = vec!["*".to_string()];
        assert!(read_jar(&jar, &all, 0.0).unwrap().is_empty(), "missing jar = no logins");

        let from_login = vec![
            json!({"name": "sid", "value": "a", "domain": ".youtube.com", "path": "/", "expires": 4e9, "size": 5, "session": false, "httpOnly": true, "secure": true, "sameSite": "Lax"}),
            json!({"name": "tmp", "value": "s", "domain": "www.instagram.com", "path": "/", "expires": -1, "session": true, "secure": true}),
            json!({"name": "part", "value": "p", "domain": ".ads.com", "path": "/", "expires": 4e9, "partitionKey": {"topLevelSite": "https://x.com"}}),
        ];
        assert_eq!(save_jar(&jar, &from_login, &all, &[]).unwrap(), 2, "partitioned cookie dropped");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&jar).unwrap().permissions().mode() & 0o777, 0o600);

        let loaded = read_jar(&jar, &all, 1.8e9).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded[0].get("size").is_none() && loaded[0].get("session").is_none(), "only CookieParam fields");
        assert!(loaded[1].get("expires").is_none(), "session cookie stays a session cookie");
        assert_eq!(read_jar(&jar, &["youtube.com".to_string()], 1.8e9).unwrap().len(), 1, "filtered by site");

        // A headless run that started signed in to YouTube, logged out (cookie gone), visited a
        // news site, and saw Instagram signed out (it must not clobber the saved Instagram login).
        let after = vec![
            json!({"name": "pref", "value": "dark", "domain": ".news.com", "path": "/", "expires": 4e9}),
            json!({"name": "anon", "value": "x", "domain": "www.instagram.com", "path": "/", "expires": 4e9}),
        ];
        save_jar(&jar, &after, &all, &["youtube.com".to_string()]).unwrap();
        let names: Vec<String> = read_jar(&jar, &all, 1.8e9).unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["tmp", "pref"], "youtube logout stuck, instagram login kept, news added");
        std::fs::remove_dir_all(dir).ok();
    }
}
