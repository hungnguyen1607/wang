//! Which desktop actions need the user's approval.
//!
//! - Reading (apps, windows, view, screenshot) and moving around are free.
//! - In apps where wang acts *as you* (chat, mail, browsers, settings, money), the browser's
//!   signed-in rules apply: buttons that aren't plainly navigation, and Enter in a text field,
//!   ask first.
//! - Elsewhere (calculator, files, editors) only clearly risky labels ask (send, delete, buy, ...).
//! - Always ask: clicks at raw x,y (pixels can't be classified) and keys that submit, delete
//!   or quit. Never: typing into password fields or terminals.

use serde_json::Value;
use wang_browser::guard::{RefInfo, assess};

/// Apps where an action can speak, pay or change things as the user.
const AS_YOU: &[&str] = &[
    "slack", "discord", "telegram", "whatsapp", "signal", "teams", "zoom", "skype", "element", "thunderbird", "evolution", "geary",
    "mail", "firefox", "chrome", "chromium", "brave", "edge", "vivaldi", "opera", "epiphany", "web", "gnome-control-center",
    "settings", "software", "snap-store", "steam", "spotify", "claude", "chatgpt", "1password", "bitwarden", "keepass", "seahorse",
];

pub fn acts_as_you(app: &str) -> bool {
    let a = app.to_lowercase();
    AS_YOU.iter().any(|k| a.contains(k))
}

/// Map an AT-SPI element (from the helper's `info`) onto the browser guard's element model.
pub fn ref_info(info: &Value) -> RefInfo {
    let role = info["role"].as_str().unwrap_or("");
    let (tag, kind, aria) = match role {
        "push button" | "button" | "menu item" | "check menu item" | "radio menu item" | "toggle button" | "check box" | "radio button" | "switch" => ("BUTTON", "", "button"),
        "link" => ("A", "", ""),
        "text" | "entry" | "password text" | "spin button" | "editbar" => ("INPUT", "", "textbox"),
        "combo box" => ("SELECT", "", ""),
        _ => ("DIV", "", ""),
    };
    RefInfo {
        tag: tag.into(),
        kind: kind.into(),
        role: aria.into(),
        text: info["text"].as_str().unwrap_or("").into(),
        // Desktop links open pages or views: treat like a plain link.
        href: if tag == "A" { "https://desktop.local/".into() } else { String::new() },
        search: info["search"] == true,
        form: false,
    }
}

/// Why `action` on this element needs approval (None = free). `Err` = never allowed.
pub fn assess_ref(action: &str, info: &Value, submit: bool) -> Result<Option<String>, String> {
    let app = info["app"].as_str().unwrap_or("");
    if info["password"] == true && action == "type" {
        return Err("wang never types into password fields".into());
    }
    if info["terminal"] == true && matches!(action, "type" | "key") {
        return Err("wang doesn't type into terminal windows; use the shell tool".into());
    }
    let r = ref_info(info);
    if acts_as_you(app) {
        return Ok(assess(action, Some(&r), submit, None).map(|why| format!("{why} in {app}")));
    }
    // Ordinary apps: only labels that clearly act (send, delete, buy, ...) ask.
    let risky = assess(action, Some(&r), submit, None).filter(|why| why.contains("looks like"));
    Ok(risky.map(|why| format!("{why} in {app}")))
}

/// Keys: submit/delete/quit combos ask (everywhere); in as-you apps any Enter asks.
pub fn assess_key(keys: &str, app: &str) -> Option<String> {
    let k = keys.to_lowercase().replace(' ', "");
    let parts: Vec<&str> = k.split('+').collect();
    let has = |x: &str| parts.contains(&x);
    let ctrl = has("ctrl") || has("control");
    let risky = (ctrl && (has("enter") || has("return") || has("w") || has("q") || has("s") || has("d") || has("x") || has("v")))
        || has("delete")
        || (has("alt") && has("f4"))
        || ((has("return") || has("enter")) && acts_as_you(app));
    risky.then(|| format!("pressing {keys} in {app}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn el(role: &str, text: &str, app: &str) -> Value {
        json!({"role": role, "text": text, "app": app, "terminal": false, "password": false, "search": false})
    }

    #[test]
    fn ordinary_apps_are_free_unless_clearly_risky() {
        for b in ["2", "+", "=", "Backspace", "Open", "Next"] {
            assert_eq!(assess_ref("click", &el("push button", b, "gnome-calculator"), false), Ok(None), "{b}");
        }
        assert!(assess_ref("click", &el("push button", "Delete", "org.gnome.Nautilus"), false).unwrap().unwrap().contains("Nautilus"));
        assert_eq!(assess_ref("type", &el("text", "", "gnome-text-editor"), false), Ok(None));
    }

    #[test]
    fn apps_that_act_as_you_use_the_signed_in_rules() {
        let slack = |r: &str, t: &str| el(r, t, "slack");
        assert!(assess_ref("click", &slack("push button", "Send now"), false).unwrap().is_some());
        assert!(assess_ref("click", &slack("push button", "Huddle"), false).unwrap().is_some(), "unknown buttons ask");
        assert_eq!(assess_ref("click", &slack("push button", "Show more"), false), Ok(None));
        assert_eq!(assess_ref("click", &slack("link", "general"), false), Ok(None), "moving to a channel is free");
        assert_eq!(assess_ref("type", &slack("text", "Message #general", ), false), Ok(None), "typing without Enter");
        assert!(assess_ref("type", &slack("text", "Message #general"), true).unwrap().unwrap().contains("Enter"));
        assert!(assess_ref("click", &el("toggle button", "Wi-Fi", "gnome-control-center"), false).unwrap().is_some());
    }

    #[test]
    fn never_passwords_or_terminals() {
        let mut p = el("password text", "Password", "seahorse");
        p["password"] = json!(true);
        assert!(assess_ref("type", &p, false).is_err());
        let mut t = el("text", "", "ptyxis");
        t["terminal"] = json!(true);
        assert!(assess_ref("type", &t, false).is_err());
        assert!(assess_ref("click", &t, false).is_ok(), "clicking a terminal's tab is fine");
    }

    #[test]
    fn keys() {
        assert!(assess_key("ctrl+q", "gnome-calculator").is_some());
        assert!(assess_key("Delete", "nautilus").is_some());
        assert!(assess_key("Return", "slack").is_some());
        assert!(assess_key("Return", "gnome-calculator").is_none());
        assert!(assess_key("ctrl+l", "firefox").is_none());
        assert!(assess_key("playpause", "spotify").is_none());
        assert!(assess_key("ctrl+Enter", "thunderbird").is_some());
    }
}
