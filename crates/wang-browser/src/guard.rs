//! Guardrails for signed-in sites: reading and moving around are free; anything that could act
//! as the user (send, post, buy, delete, change settings) needs their approval first.
//!
//! This matters because a page can carry instructions meant for the agent. On a site where
//! wang is signed in with the user's cookies, the approval is what stops a page from making
//! it post, pay or delete as them.

use serde::Deserialize;

/// What an element in the page view is, for deciding whether acting on it is risky.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RefInfo {
    #[serde(default)]
    pub tag: String,
    /// `type` attribute (submit, search, ...).
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub role: String,
    /// Visible text or accessible label.
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub href: String,
    /// A search box (type=search, role=searchbox, or named/labelled like one).
    #[serde(default)]
    pub search: bool,
}

/// Reads `__wangRefs[n-1]` (from the last view). Returns null if the element is gone.
pub const REF_INFO_JS: &str = r#"(n) => {
  const el = (window.__wangRefs || [])[n - 1];
  if (!el || !el.isConnected) return null;
  const clean = (s) => (s || "").replace(/\s+/g, " ").trim().slice(0, 120);
  const attr = (k) => el.getAttribute(k) || "";
  const text = clean(attr("aria-label") || el.innerText || el.value || attr("title") || attr("alt") || attr("placeholder"));
  const hint = [attr("name"), el.id, attr("aria-label"), attr("placeholder"), attr("title")].join(" ").toLowerCase();
  const search = attr("type") === "search" || attr("role") === "searchbox" || !!el.closest("[role=search]") ||
                 /\b(search|query)\b/.test(hint) || attr("name") === "q";
  return {tag: el.tagName, type: (attr("type") || "").toLowerCase(), role: attr("role").toLowerCase(),
          text, href: el.tagName === "A" ? (el.href || "") : "", search};
}"#;

/// Words that mean "this acts as you". Matched as whole words in the element's label.
const RISKY: &[&str] = &[
    "send", "post", "publish", "share", "repost", "retweet", "tweet", "reply", "comment", "message", "dm",
    "buy", "purchase", "pay", "order", "checkout", "donate", "transfer", "withdraw", "subscribe", "unsubscribe", "book", "reserve", "rsvp",
    "delete", "remove", "trash", "archive", "deactivate", "close account", "cancel subscription", "cancel order",
    "confirm", "submit", "accept", "approve", "decline", "invite", "upload", "save", "update", "apply", "change", "edit",
    "follow", "unfollow", "block", "unblock", "mute account", "report", "like", "unlike", "react", "vote",
    "add to cart", "add to bag", "place order", "sign out", "log out", "logout", "signout",
];

/// Buttons that only move around or change what's shown.
const SAFE_BUTTONS: &[&str] = &[
    "search", "play", "pause", "next", "previous", "prev", "more", "show more", "see more", "load more", "read more", "expand",
    "collapse", "close", "dismiss", "not now", "menu", "back", "forward", "filter", "filters", "sort", "skip", "skip ad",
    "skip ads", "fullscreen", "full screen", "captions", "subtitles", "unmute", "volume", "zoom", "view", "show", "hide",
    "open", "options", "home", "explore", "notifications", "profile", "settings", "help",
];

/// URL paths that act just by being opened.
const RISKY_URL_WORDS: &[&str] = &["logout", "log-out", "signout", "sign-out", "sign_out", "delete", "unsubscribe", "remove", "deactivate"];

fn words(s: &str) -> String {
    let w: String = s.to_lowercase().chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect();
    format!(" {} ", w.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn has_word(label: &str, list: &[&'static str]) -> Option<&'static str> {
    let w = words(label);
    list.iter().copied().find(|p| w.contains(&format!(" {p} ")))
}

/// Why this action on a signed-in page needs approval, or None if it's free.
/// `info` is None when the element can't be read (the action will then fail on its own).
pub fn assess(action: &str, info: Option<&RefInfo>, submit: bool, open_url: Option<&str>) -> Option<String> {
    match action {
        "open" => {
            let u = open_url?.to_lowercase();
            let path = u.split_once("://").map(|(_, r)| r.split_once('/').map(|(_, p)| p).unwrap_or("")).unwrap_or(&u);
            RISKY_URL_WORDS.iter().find(|w| path.contains(*w)).map(|w| format!("opening a URL that looks like it acts by itself (\"{w}\")"))
        }
        "click" => {
            let i = info?;
            if let Some(w) = has_word(&i.text, RISKY) {
                return Some(format!("clicking \"{}\" (looks like \"{w}\")", i.text));
            }
            let plain_link = i.tag == "A" && (i.href.starts_with("http://") || i.href.starts_with("https://")) && i.role != "button";
            if plain_link {
                return None;
            }
            if i.kind == "submit" || i.tag == "BUTTON" || i.role == "button" || i.tag == "INPUT" || i.role == "menuitem" {
                if has_word(&i.text, SAFE_BUTTONS).is_some() && i.kind != "submit" {
                    return None;
                }
                if i.search {
                    return None;
                }
                return Some(format!("clicking the button \"{}\"", if i.text.is_empty() { "(unlabelled)" } else { &i.text }));
            }
            None
        }
        "type" => {
            let i = info?;
            if !submit || i.search {
                return None;
            }
            Some(format!("typing into \"{}\" and pressing Enter (it may send or submit)", if i.text.is_empty() { "a field" } else { &i.text }))
        }
        "select" => {
            let i = info?;
            has_word(&i.text, RISKY).map(|w| format!("changing \"{}\" (looks like \"{w}\")", i.text))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(tag: &str, kind: &str, role: &str, text: &str, href: &str, search: bool) -> RefInfo {
        RefInfo { tag: tag.into(), kind: kind.into(), role: role.into(), text: text.into(), href: href.into(), search }
    }

    #[test]
    fn reading_and_moving_are_free() {
        assert_eq!(assess("click", Some(&el("A", "", "", "Lo-fi beats to study to", "https://www.youtube.com/watch?v=x", false)), false, None), None);
        assert_eq!(assess("click", Some(&el("BUTTON", "", "", "Show more", "", false)), false, None), None);
        assert_eq!(assess("click", Some(&el("BUTTON", "", "", "Search", "", false)), false, None), None);
        assert_eq!(assess("click", Some(&el("BUTTON", "", "", "", "", true)), false, None), None, "search box button");
        assert_eq!(assess("type", Some(&el("INPUT", "search", "", "Search", "", true)), true, None), None);
        assert_eq!(assess("type", Some(&el("TEXTAREA", "", "", "Message...", "", false)), false, None), None, "typing without Enter");
        assert_eq!(assess("open", None, false, Some("https://www.instagram.com/natgeo/")), None);
        assert_eq!(assess("view", None, false, None), None);
        assert_eq!(assess("back", None, false, None), None);
    }

    #[test]
    fn acting_as_you_needs_approval() {
        for (tag, kind, role, text) in [
            ("BUTTON", "", "", "Send"),
            ("DIV", "", "button", "Post"),
            ("BUTTON", "", "", "Place order"),
            ("A", "", "", "Delete account"),
            ("BUTTON", "", "", "Follow"),
            ("INPUT", "submit", "", "Search"), // a submit button still submits a form
            ("BUTTON", "", "", "Like"),
            ("BUTTON", "", "", ""),
            ("BUTTON", "", "", "Continue"),
        ] {
            assert!(assess("click", Some(&el(tag, kind, role, text, "", false)), false, None).is_some(), "{tag} {text}");
        }
        assert!(assess("type", Some(&el("TEXTAREA", "", "", "Message...", "", false)), true, None).unwrap().contains("pressing Enter"));
        assert!(assess("open", None, false, Some("https://accounts.google.com/Logout?continue=x")).is_some());
        assert!(assess("select", Some(&el("SELECT", "", "", "Change plan", "", false)), false, None).is_some());
    }

    #[test]
    fn words_match_whole_words_only() {
        assert!(has_word("Unlikely result", RISKY).is_none(), "'like' isn't in 'unlikely'");
        assert!(has_word("Resend code", RISKY).is_none());
        assert_eq!(has_word("Add to cart", RISKY), Some("add to cart"));
        assert_eq!(has_word("Log out", RISKY), Some("log out"));
    }
}
