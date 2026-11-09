// wang compact page view: the visible page as plain text with numbered interactive elements.
// Returns {text, refs, html_bytes, text_bytes}. Elements are kept in window.__wangRefs so later
// actions can find them by number. Runs as `(VIEW)(maxChars)`.
(maxChars) => {
  const refs = [];
  window.__wangRefs = refs;
  const lines = [];
  let line = "";
  let total = 0;
  let truncated = false;
  const seen = new Set();

  const SKIP = new Set(["SCRIPT", "STYLE", "NOSCRIPT", "TEMPLATE", "HEAD", "META", "LINK", "SVG", "CANVAS", "OBJECT", "EMBED"]);
  const BLOCK = new Set(["ADDRESS", "ARTICLE", "ASIDE", "BLOCKQUOTE", "BR", "DD", "DETAILS", "DIALOG", "DIV", "DL", "DT",
    "FIELDSET", "FIGCAPTION", "FIGURE", "FOOTER", "FORM", "H1", "H2", "H3", "H4", "H5", "H6", "HEADER", "HR", "LI", "MAIN",
    "NAV", "OL", "P", "PRE", "SECTION", "SUMMARY", "TABLE", "TBODY", "THEAD", "TFOOT", "TR", "UL", "CAPTION", "LEGEND"]);

  const clean = (s) => (s || "").replace(/\s+/g, " ").trim();
  const clip = (s, n) => (s.length > n ? s.slice(0, n - 1) + "…" : s);

  const flush = () => {
    const l = line.trim();
    line = "";
    if (!l) return;
    // Repeated plain lines (menus, footers echoed on the page) are shown once.
    if (!l.includes("[") && seen.has(l)) return;
    seen.add(l);
    if (total + l.length > maxChars) { truncated = true; return; }
    lines.push(l);
    total += l.length + 1;
  };
  const emit = (s) => {
    if (!s) return;
    if (line && !line.endsWith(" ") && !/^[,.;:!?)]/.test(s)) line += " ";
    line += s;
  };

  const visible = (el) => {
    if (el.checkVisibility && !el.checkVisibility({ opacityProperty: true, visibilityProperty: true })) {
      // `display: contents` has no box of its own, but its children render.
      if (getComputedStyle(el).display !== "contents") return false;
    }
    if (el.getAttribute("aria-hidden") === "true") return false;
    return true;
  };

  // Links cost tokens: same-site links show no URL (click them by number; the view's header
  // shows where you landed), other sites show their host.
  const shortHref = (a) => {
    try {
      const u = new URL(a.href, location.href);
      if (u.protocol !== "http:" && u.protocol !== "https:") return "";
      if (u.host === location.host) return "";
      return u.host.replace(/^www\./, "");
    } catch { return ""; }
  };

  const label = (el) => {
    const aria = el.getAttribute("aria-label") || el.getAttribute("title");
    if (aria) return clean(aria);
    if (el.id) {
      const l = document.querySelector(`label[for="${CSS.escape(el.id)}"]`);
      if (l) return clean(l.innerText);
    }
    const parentLabel = el.closest("label");
    if (parentLabel) return clean(parentLabel.innerText);
    return "";
  };

  const ref = (el) => { refs.push(el); return refs.length; };

  const textOf = (el) => {
    let t = clean(el.innerText);
    if (!t) {
      const img = el.querySelector("img[alt]");
      if (img) t = clean(img.alt);
    }
    if (!t) t = clean(el.getAttribute("aria-label") || el.getAttribute("title") || "");
    return clip(t, 100);
  };

  const control = (el) => {
    const tag = el.tagName;
    if (tag === "A") {
      const t = textOf(el) || "(link)";
      const h = shortHref(el);
      emit(`[${ref(el)} ${t}${h ? " → " + h : ""}]`);
      return true;
    }
    if (tag === "BUTTON" || (tag === "INPUT" && ["submit", "button", "reset", "image"].includes(el.type))) {
      const t = textOf(el) || clean(el.value) || el.type;
      emit(`[${ref(el)} button: ${t}${el.disabled ? " (disabled)" : ""}]`);
      return true;
    }
    if (tag === "INPUT") {
      if (el.type === "hidden") return true;
      const l = label(el) || clean(el.placeholder) || el.name || "";
      if (el.type === "checkbox" || el.type === "radio") {
        emit(`[${ref(el)} ${el.type} ${el.checked ? "☑" : "☐"} ${clip(l, 80)}]`);
        return true;
      }
      const v = el.type === "password" ? (el.value ? "•••" : "") : clip(clean(el.value), 60);
      emit(`[${ref(el)} input ${el.type}${l ? ' "' + clip(l, 60) + '"' : ""}${v ? " = " + v : ""}]`);
      return true;
    }
    if (tag === "TEXTAREA") {
      const l = label(el) || clean(el.placeholder) || el.name || "";
      emit(`[${ref(el)} textarea${l ? ' "' + clip(l, 60) + '"' : ""}${el.value ? " = " + clip(clean(el.value), 80) : ""}]`);
      return true;
    }
    if (tag === "SELECT") {
      const opts = [...el.options].map((o) => clip(clean(o.text), 30));
      const cur = el.selectedIndex >= 0 ? opts[el.selectedIndex] : "";
      const shown = opts.slice(0, 12).join(" | ") + (opts.length > 12 ? ` | … ${opts.length - 12} more` : "");
      emit(`[${ref(el)} select${label(el) ? ' "' + clip(label(el), 40) + '"' : ""} = ${cur} (${shown})]`);
      return true;
    }
    const role = el.getAttribute("role");
    if (role === "button" || role === "link" || role === "tab" || role === "menuitem" || role === "checkbox" || role === "switch" || tag === "SUMMARY") {
      emit(`[${ref(el)} ${role || "toggle"}: ${textOf(el)}]`);
      return true;
    }
    if (el.isContentEditable && (el.parentElement && !el.parentElement.isContentEditable)) {
      emit(`[${ref(el)} editable: ${clip(clean(el.innerText), 80)}]`);
      return true;
    }
    return false;
  };

  const walk = (node, depth) => {
    if (truncated || depth > 200) return;
    if (node.nodeType === Node.TEXT_NODE) {
      emit(clean(node.textContent));
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return;
    const el = node;
    const tag = el.tagName;
    if (SKIP.has(tag) || !visible(el)) return;
    if (tag === "IFRAME") { flush(); emit("[iframe]"); flush(); return; }
    if (tag === "IMG") {
      const alt = clean(el.alt);
      if (alt && alt.length > 1) emit(`[img: ${clip(alt, 80)}]`);
      return;
    }
    // A label is printed as part of its control ([n input "Label"]), not twice.
    if (tag === "LABEL" && el.control) {
      if (el.contains(el.control)) walk(el.control, depth + 1);
      return;
    }
    const block = BLOCK.has(tag);
    if (block) flush();
    if (control(el)) { if (block) flush(); return; }
    if (/^H[1-6]$/.test(tag)) emit("#".repeat(+tag[1]));
    else if (tag === "LI") emit("-");
    else if (tag === "HR") emit("---");
    if (tag === "TR") {
      const cells = [...el.children].filter((c) => c.tagName === "TD" || c.tagName === "TH");
      cells.forEach((c, i) => { if (i) emit("|"); for (const k of c.childNodes) walk(k, depth + 1); });
      flush();
      return;
    }
    // Web components: walk the shadow tree; its <slot>s stand for the light-DOM children.
    let kids = el.shadowRoot ? el.shadowRoot.childNodes : el.childNodes;
    if (tag === "SLOT") {
      const assigned = el.assignedNodes({ flatten: true });
      if (assigned.length) kids = assigned;
    }
    for (const k of kids) walk(k, depth + 1);
    if (block) flush();
  };

  if (document.body) walk(document.body, 0);
  flush();
  const header = `${clean(document.title) || "(untitled)"}\n${location.href}`;
  const html = document.documentElement ? document.documentElement.outerHTML.length : 0;
  const text = document.body ? document.body.innerText.length : 0;
  return {
    text: header + "\n\n" + lines.join("\n") + (truncated ? `\n[... page cut at ${maxChars} chars ...]` : ""),
    refs: refs.length,
    html_bytes: html,
    text_bytes: text,
  };
}
