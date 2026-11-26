//! Pure-Rust HTML5 extraction, Markdown conversion, sitemap parsing, and browser-escalation detection.

use std::collections::HashSet;

use scraper::{ElementRef, Html, Node, Selector};
use url::Url;

const MAX_EXTRACTED_LINKS: usize = 5000;
const SPA_MIN_TEXT_CHARS: usize = 200;

const SKIP_TAGS: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "canvas", "iframe", "head", "object", "embed",
];

const BLOCK_TAGS: &[&str] = &[
    "p",
    "div",
    "section",
    "article",
    "main",
    "header",
    "footer",
    "nav",
    "aside",
    "figcaption",
    "figure",
];

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct FastHtmlExtraction {
    pub(crate) title: String,
    pub(crate) text: String,
    pub(crate) markdown: String,
    pub(crate) links: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct FastScrapeFields {
    pub(crate) title: String,
    pub(crate) text: Option<String>,
    pub(crate) markdown: Option<String>,
}

/// Decodes response bytes to a string, honoring explicit ISO-8859-1/Latin-1 charset if declared.
pub(crate) fn decode_body_text(bytes: &[u8], content_type: Option<&str>) -> String {
    if let Some(ct) = content_type {
        let lower = ct.to_ascii_lowercase();
        if let Some(pos) = lower.find("charset=") {
            let cs = lower[pos + 8..]
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"')
                .trim_matches('\'');
            if matches!(cs, "iso-8859-1" | "latin1" | "latin-1") {
                return bytes.iter().map(|&b| b as char).collect();
            }
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Inspects status, headers, Content-Type, challenge markers, and DOM shell structure to decide
/// whether the response should escalate to Chromium. Returns `Some(reason)` when escalation is needed.
pub(crate) fn detect_escalation(
    status: u16,
    headers: &[(String, String)],
    content_type: Option<&str>,
    body_text: &str,
) -> Option<String> {
    if headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("cf-mitigated"))
    {
        return Some("challenge header detected: cf-mitigated".into());
    }

    if matches!(status, 401 | 403 | 429) || (500..=599).contains(&status) {
        return Some(format!("http status {status}"));
    }

    if let Some(ct) = content_type {
        let mime = ct
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if !mime.is_empty() && mime != "text/html" && mime != "application/xhtml+xml" {
            return Some(format!("non-html content-type: {mime}"));
        }
    } else {
        let trimmed = body_text.trim_start();
        if !trimmed.is_empty() && !trimmed.starts_with('<') {
            return Some("non-html response body".into());
        }
    }

    if let Some(marker) = find_challenge_marker(headers, body_text) {
        return Some(format!("challenge marker detected: {marker}"));
    }

    let doc = Html::parse_document(body_text);
    detect_spa_shell(&doc)
}

fn find_challenge_marker(headers: &[(String, String)], body_text: &str) -> Option<&'static str> {
    for (k, v) in headers {
        if k.eq_ignore_ascii_case("cf-mitigated") || v.to_ascii_lowercase().contains("cf-mitigated")
        {
            return Some("cf-mitigated");
        }
        if v.to_ascii_lowercase().contains("ddos-guard") {
            return Some("ddos-guard");
        }
    }
    let lower = body_text.to_ascii_lowercase();
    for (needle, label) in [
        ("cf-mitigated", "cf-mitigated"),
        ("just a moment", "Just a moment"),
        ("/cdn-cgi/challenge-platform", "/cdn-cgi/challenge-platform"),
        ("_incapsula_resource", "_Incapsula_Resource"),
        ("ddos-guard", "ddos-guard"),
    ] {
        if lower.contains(needle) {
            return Some(label);
        }
    }
    None
}

fn detect_spa_shell(doc: &Html) -> Option<String> {
    let noscript_sel = Selector::parse("noscript").expect("valid noscript selector");
    for ns in doc.select(&noscript_sel) {
        let msg = clean_inline(&ns.text().collect::<String>()).to_ascii_lowercase();
        if noscript_requires_js(&msg) {
            return Some("unhydrated SPA shell (<noscript> requires JavaScript)".into());
        }
    }

    let spa_root_sel =
        Selector::parse("#root, #app, #__next, #__nuxt").expect("valid spa root selector");
    let script_sel = Selector::parse("script").expect("valid script selector");
    let has_spa_root = doc.select(&spa_root_sel).next().is_some();
    if has_spa_root && doc.select(&script_sel).next().is_some() {
        let readable = extract_plain_text_from_doc(doc);
        if readable.chars().count() < SPA_MIN_TEXT_CHARS {
            return Some(format!(
                "unhydrated SPA shell (mount root with <script> and {} chars readable text)",
                readable.chars().count()
            ));
        }
    }
    None
}

fn noscript_requires_js(lower_msg: &str) -> bool {
    if lower_msg.is_empty() {
        return false;
    }
    let mentions_js = lower_msg.contains("javascript") || lower_msg.contains("enable js");
    if !mentions_js {
        return false;
    }
    lower_msg.contains("enable")
        || lower_msg.contains("require")
        || lower_msg.contains("turn on")
        || lower_msg.contains("activate")
        || lower_msg.contains("need")
        || lower_msg.contains("doesn't work")
        || lower_msg.contains("does not work")
        || lower_msg.contains("must be")
        || lower_msg.contains("disabled")
}

/// Parses HTML once and extracts title, plain text, Markdown, and resolved HTTP(S) `a[href]` links.
#[cfg(test)]
pub(crate) fn extract_html(html: &str, page_url: &str) -> FastHtmlExtraction {
    let doc = Html::parse_document(html);
    let base_url = resolve_document_base(&doc, page_url);

    let title = extract_title(&doc);
    let text = extract_plain_text_from_doc(&doc);
    let markdown = extract_markdown_from_doc(&doc, &base_url, false, true, false);
    let links = extract_links_from_doc(&doc, &base_url);

    FastHtmlExtraction {
        title,
        text,
        markdown,
        links,
    }
}

/// Parses HTML once and extracts `<title>` plus only the requested `text`/`markdown` formats.
pub(crate) fn extract_scrape_fields(
    html: &str,
    page_url: &str,
    want_markdown: bool,
    want_text: bool,
) -> FastScrapeFields {
    let doc = Html::parse_document(html);
    let base_url = resolve_document_base(&doc, page_url);
    let title = extract_title(&doc);
    let text = want_text.then(|| extract_plain_text_from_doc(&doc));
    let markdown =
        want_markdown.then(|| extract_markdown_from_doc(&doc, &base_url, false, true, false));
    FastScrapeFields {
        title,
        text,
        markdown,
    }
}

/// Converts HTML into Markdown matching `crates/fluxwright-cdp/src/markdown.js` semantics.
#[cfg(test)]
pub(crate) fn html_to_markdown(
    html: &str,
    page_url: &str,
    main_content_only: bool,
    include_links: bool,
    include_images: bool,
) -> String {
    let doc = Html::parse_document(html);
    let base_url = resolve_document_base(&doc, page_url);
    extract_markdown_from_doc(
        &doc,
        &base_url,
        main_content_only,
        include_links,
        include_images,
    )
}

/// Extracts unique absolute `http://`/`https://` URLs from `<a href>` elements.
pub(crate) fn extract_links(html: &str, page_url: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let base_url = resolve_document_base(&doc, page_url);
    extract_links_from_doc(&doc, &base_url)
}

/// Extracts `<loc>` HTTP(S) URLs from XML sitemaps (`<urlset>` or `<sitemapindex>`).
pub(crate) fn parse_sitemap_locs(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut pos = 0usize;
    let bytes = xml.as_bytes();

    while pos < bytes.len() {
        let Some(rel_open) = xml[pos..].find('<') else {
            break;
        };
        let tag_start = pos + rel_open;
        if xml[tag_start..].starts_with("<!--") {
            if let Some(end) = xml[tag_start + 4..].find("-->") {
                pos = tag_start + 4 + end + 3;
                continue;
            }
            break;
        }
        if xml[tag_start..].starts_with("<![CDATA[") {
            if let Some(end) = xml[tag_start + 9..].find("]]>") {
                pos = tag_start + 9 + end + 3;
                continue;
            }
            break;
        }
        let Some(rel_gt) = xml[tag_start..].find('>') else {
            break;
        };
        let tag_end = tag_start + rel_gt;
        let tag_inner = xml[tag_start + 1..tag_end].trim();
        let tag_name = tag_inner
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        let local_name = tag_name.rsplit(':').next().unwrap_or(tag_name);

        if !tag_inner.starts_with('/')
            && !tag_inner.ends_with('/')
            && local_name.eq_ignore_ascii_case("loc")
        {
            let content_start = tag_end + 1;
            if let Some((content_end, next_pos)) = find_closing_loc(xml, content_start) {
                let raw_val = &xml[content_start..content_end];
                let unescaped = decode_xml_loc_value(raw_val);
                if let Ok(parsed) = Url::parse(&unescaped) {
                    if matches!(parsed.scheme(), "http" | "https") {
                        let s = parsed.to_string();
                        if seen.insert(s.clone()) {
                            out.push(s);
                            if out.len() >= MAX_EXTRACTED_LINKS {
                                break;
                            }
                        }
                    }
                }
                pos = next_pos;
                continue;
            }
        }
        pos = tag_end + 1;
    }
    out
}

fn find_closing_loc(xml: &str, start: usize) -> Option<(usize, usize)> {
    let mut cursor = start;
    while cursor < xml.len() {
        let rel = xml[cursor..].find('<')?;
        let tag_start = cursor + rel;
        if xml[tag_start..].starts_with("<![CDATA[") {
            let end = xml[tag_start + 9..].find("]]>")?;
            cursor = tag_start + 9 + end + 3;
            continue;
        }
        let rel_gt = xml[tag_start..].find('>')?;
        let tag_end = tag_start + rel_gt;
        let inner = xml[tag_start + 1..tag_end].trim();
        if let Some(after_slash) = inner.strip_prefix('/') {
            let name = after_slash.split_whitespace().next().unwrap_or("");
            let local = name.rsplit(':').next().unwrap_or(name);
            if local.eq_ignore_ascii_case("loc") {
                return Some((tag_start, tag_end + 1));
            }
        }
        cursor = tag_end + 1;
    }
    None
}

fn decode_xml_loc_value(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    if let Some(stripped) = s
        .strip_prefix("<![CDATA[")
        .and_then(|v| v.strip_suffix("]]>"))
    {
        s = stripped.trim().to_string();
    }
    if !s.contains('&') {
        return s;
    }
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

fn resolve_document_base(doc: &Html, page_url: &str) -> Option<Url> {
    let base_page = Url::parse(page_url).ok();
    let base_sel = Selector::parse("base[href]").expect("valid base selector");
    if let Some(base_el) = doc.select(&base_sel).next() {
        if let Some(href) = base_el.value().attr("href") {
            let trimmed = href.trim();
            if !trimmed.is_empty() {
                if let Some(ref page) = base_page {
                    if let Ok(joined) = page.join(trimmed) {
                        if matches!(joined.scheme(), "http" | "https") {
                            return Some(joined);
                        }
                    }
                } else if let Ok(abs) = Url::parse(trimmed) {
                    return Some(abs);
                }
            }
        }
    }
    base_page
}

fn extract_title(doc: &Html) -> String {
    let head_title_sel = Selector::parse("head title").expect("valid head title selector");
    if let Some(el) = doc.select(&head_title_sel).next() {
        return clean_inline(&el.text().collect::<String>());
    }
    let title_sel = Selector::parse("title").expect("valid title selector");
    for el in doc.select(&title_sel) {
        let inside_svg = el
            .ancestors()
            .filter_map(ElementRef::wrap)
            .any(|anc| anc.value().name().eq_ignore_ascii_case("svg"));
        if !inside_svg {
            return clean_inline(&el.text().collect::<String>());
        }
    }
    String::new()
}

fn extract_links_from_doc(doc: &Html, base_url: &Option<Url>) -> Vec<String> {
    let a_sel = Selector::parse("a[href]").expect("valid a[href] selector");
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for a in doc.select(&a_sel) {
        let Some(raw_href) = a.value().attr("href") else {
            continue;
        };
        let trimmed = raw_href.trim();
        let resolved = match base_url {
            Some(base) => base.join(trimmed),
            None => Url::parse(trimmed),
        };
        let Ok(url) = resolved else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") {
            continue;
        }
        let s = url.to_string();
        if seen.insert(s.clone()) {
            out.push(s);
            if out.len() >= MAX_EXTRACTED_LINKS {
                break;
            }
        }
    }
    out
}

fn extract_plain_text_from_doc(doc: &Html) -> String {
    let body_sel = Selector::parse("body").expect("valid body selector");
    let root = doc
        .select(&body_sel)
        .next()
        .unwrap_or_else(|| doc.root_element());

    let mut raw = String::new();
    collect_plain_text(root, &mut raw, false);
    normalize_newlines(&raw)
}

fn collect_plain_text(el: ElementRef<'_>, out: &mut String, in_pre: bool) {
    let tag = el.value().name().to_ascii_lowercase();
    if is_skip_tag(&tag) || is_hidden(el) {
        return;
    }
    if tag == "br" {
        out.push('\n');
        return;
    }
    let is_block = is_block_tag(&tag)
        || matches!(
            tag.as_str(),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "li" | "tr" | "blockquote" | "pre" | "hr"
        );
    if is_block && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    let child_in_pre = in_pre || tag == "pre";
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                if child_in_pre {
                    out.push_str(&**t);
                } else {
                    let collapsed = collapse_ws_preserve_edges(&**t);
                    let trimmed = if out.is_empty() || out.ends_with('\n') {
                        collapsed.trim_start()
                    } else {
                        collapsed.as_str()
                    };
                    if !trimmed.is_empty() {
                        if trimmed == " " && out.ends_with(' ') {
                            continue;
                        }
                        out.push_str(trimmed);
                    }
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    collect_plain_text(child_el, out, child_in_pre);
                }
            }
            _ => {}
        }
    }
    if is_block && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn extract_markdown_from_doc(
    doc: &Html,
    base_url: &Option<Url>,
    main_content_only: bool,
    include_links: bool,
    include_images: bool,
) -> String {
    let root = pick_markdown_root(doc, main_content_only);
    let raw = serialize_element(root, 0, false, base_url, include_links, include_images);
    normalize_newlines(&raw)
}

fn pick_markdown_root<'a>(doc: &'a Html, main_content_only: bool) -> ElementRef<'a> {
    if main_content_only {
        let main_sel =
            Selector::parse("main, article, [role=\"main\"]").expect("valid main selector");
        if let Some(el) = doc.select(&main_sel).next() {
            return el;
        }
    }
    let body_sel = Selector::parse("body").expect("valid body selector");
    doc.select(&body_sel)
        .next()
        .unwrap_or_else(|| doc.root_element())
}

fn serialize_children(
    el: ElementRef<'_>,
    list_depth: usize,
    in_pre: bool,
    base_url: &Option<Url>,
    include_links: bool,
    include_images: bool,
) -> String {
    let mut out = String::new();
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                if in_pre {
                    out.push_str(&**t);
                } else {
                    out.push_str(&collapse_ws_preserve_edges(&**t));
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    out.push_str(&serialize_element(
                        child_el,
                        list_depth,
                        in_pre,
                        base_url,
                        include_links,
                        include_images,
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

fn serialize_element(
    el: ElementRef<'_>,
    list_depth: usize,
    in_pre: bool,
    base_url: &Option<Url>,
    include_links: bool,
    include_images: bool,
) -> String {
    let tag = el.value().name().to_ascii_lowercase();
    if is_skip_tag(&tag) || is_hidden(el) {
        return String::new();
    }

    match tag.as_str() {
        "br" => "\n".to_string(),
        "hr" => "\n\n---\n\n".to_string(),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = usize::from(tag.as_bytes()[1] - b'0');
            let text = clean_inline(&serialize_children(
                el,
                list_depth,
                false,
                base_url,
                include_links,
                include_images,
            ));
            if text.is_empty() {
                String::new()
            } else {
                format!("\n\n{} {}\n\n", "#".repeat(level), text)
            }
        }
        "pre" => {
            let code_sel = Selector::parse("code").expect("valid code selector");
            let target = el.select(&code_sel).next().unwrap_or(el);
            let raw = target
                .text()
                .collect::<String>()
                .trim_matches(|c| c == '\n' || c == '\r')
                .to_string();
            if raw.is_empty() {
                String::new()
            } else {
                format!("\n\n```\n{raw}\n```\n\n")
            }
        }
        "code" => {
            let text = el.text().collect::<String>().trim().to_string();
            if text.is_empty() {
                String::new()
            } else {
                format!("`{text}`")
            }
        }
        "strong" | "b" => {
            let text = clean_inline(&serialize_children(
                el,
                list_depth,
                in_pre,
                base_url,
                include_links,
                include_images,
            ));
            if text.is_empty() {
                String::new()
            } else {
                format!("**{text}**")
            }
        }
        "em" | "i" => {
            let text = clean_inline(&serialize_children(
                el,
                list_depth,
                in_pre,
                base_url,
                include_links,
                include_images,
            ));
            if text.is_empty() {
                String::new()
            } else {
                format!("*{text}*")
            }
        }
        "a" => {
            let text = clean_inline(&serialize_children(
                el,
                list_depth,
                in_pre,
                base_url,
                include_links,
                include_images,
            ));
            let raw_href = el.value().attr("href").unwrap_or("").trim();
            if !include_links
                || raw_href.is_empty()
                || raw_href.to_ascii_lowercase().starts_with("javascript:")
            {
                return text;
            }
            let href = match base_url {
                Some(base) => base
                    .join(raw_href)
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| raw_href.to_string()),
                None => raw_href.to_string(),
            };
            if text.is_empty() {
                href
            } else {
                format!("[{text}]({href})")
            }
        }
        "img" => {
            if !include_images {
                return String::new();
            }
            let raw_src = el.value().attr("src").unwrap_or("").trim();
            let src = if raw_src.is_empty() {
                String::new()
            } else if let Some(base) = base_url {
                base.join(raw_src)
                    .map(|u| u.to_string())
                    .unwrap_or_else(|_| raw_src.to_string())
            } else {
                raw_src.to_string()
            };
            let alt = clean_inline(el.value().attr("alt").unwrap_or(""));
            if src.is_empty() && alt.is_empty() {
                String::new()
            } else {
                format!("![{alt}]({src})")
            }
        }
        "ul" | "ol" => serialize_list(el, list_depth, base_url, include_links, include_images),
        "table" => serialize_table(el, base_url, include_links, include_images),
        "blockquote" => {
            let body = serialize_children(
                el,
                list_depth,
                false,
                base_url,
                include_links,
                include_images,
            );
            let trimmed = body.trim();
            if trimmed.is_empty() {
                String::new()
            } else {
                let quoted = trimmed
                    .lines()
                    .map(|l| format!("> {}", l.trim()))
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("\n\n{quoted}\n\n")
            }
        }
        _ => {
            let inner = serialize_children(
                el,
                list_depth,
                in_pre,
                base_url,
                include_links,
                include_images,
            );
            if is_block_tag(&tag) {
                let trimmed = inner.trim();
                if trimmed.is_empty() {
                    String::new()
                } else {
                    format!("\n\n{trimmed}\n\n")
                }
            } else {
                inner
            }
        }
    }
}

fn serialize_list(
    list_el: ElementRef<'_>,
    depth: usize,
    base_url: &Option<Url>,
    include_links: bool,
    include_images: bool,
) -> String {
    let ordered = list_el.value().name().eq_ignore_ascii_case("ol");
    let indent = "  ".repeat(depth);
    let mut idx = list_el
        .value()
        .attr("start")
        .and_then(|s| s.trim().parse::<isize>().ok())
        .unwrap_or(1);

    let mut lines = Vec::new();
    for child in list_el.child_elements() {
        if !child.value().name().eq_ignore_ascii_case("li") || is_hidden(child) {
            continue;
        }
        let prefix = if ordered {
            let cur = idx;
            idx += 1;
            format!("{cur}. ")
        } else {
            "- ".to_string()
        };

        let mut parts = Vec::new();
        let mut sub_lists = Vec::new();
        for node in child.children() {
            match node.value() {
                Node::Text(t) => parts.push(collapse_ws_preserve_edges(&**t)),
                Node::Element(_) => {
                    if let Some(el) = ElementRef::wrap(node) {
                        let name = el.value().name().to_ascii_lowercase();
                        if name == "ul" || name == "ol" {
                            sub_lists.push(serialize_list(
                                el,
                                depth + 1,
                                base_url,
                                include_links,
                                include_images,
                            ));
                        } else {
                            parts.push(serialize_element(
                                el,
                                depth + 1,
                                false,
                                base_url,
                                include_links,
                                include_images,
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
        let joined = parts.join("");
        let line = collapse_newlines_to_space(&joined);
        if !line.is_empty() {
            lines.push(format!("{indent}{prefix}{line}"));
        }
        for sub in sub_lists {
            if !sub.is_empty() {
                lines.push(sub);
            }
        }
    }

    if lines.is_empty() {
        String::new()
    } else {
        format!("\n\n{}\n\n", lines.join("\n"))
    }
}

fn serialize_table(
    table: ElementRef<'_>,
    base_url: &Option<Url>,
    include_links: bool,
    include_images: bool,
) -> String {
    let tr_sel = Selector::parse("tr").expect("valid tr selector");
    let mut matrix = Vec::new();

    for tr in table.select(&tr_sel) {
        if is_hidden(tr) {
            continue;
        }
        let mut row = Vec::new();
        for cell in tr.child_elements() {
            let name = cell.value().name().to_ascii_lowercase();
            if (name == "th" || name == "td") && !is_hidden(cell) {
                let cell_md =
                    serialize_children(cell, 0, false, base_url, include_links, include_images);
                let cleaned = clean_inline(&cell_md)
                    .replace('\\', "\\\\")
                    .replace('|', "\\|");
                row.push(cleaned);
            }
        }
        if !row.is_empty() {
            matrix.push(row);
        }
    }

    if matrix.is_empty() {
        return String::new();
    }

    let cols = matrix.iter().map(Vec::len).max().unwrap_or(0);
    let pad_row = |r: &[String]| -> String {
        let mut cells: Vec<String> = r.to_vec();
        while cells.len() < cols {
            cells.push(String::new());
        }
        format!("| {} |", cells.join(" | "))
    };

    let mut out = Vec::with_capacity(matrix.len() + 1);
    out.push(pad_row(&matrix[0]));
    out.push(format!("| {} |", vec!["---"; cols].join(" | ")));
    for row in matrix.iter().skip(1) {
        out.push(pad_row(row));
    }
    format!("\n\n{}\n\n", out.join("\n"))
}

fn is_skip_tag(tag: &str) -> bool {
    SKIP_TAGS.contains(&tag)
}

fn is_block_tag(tag: &str) -> bool {
    BLOCK_TAGS.contains(&tag)
}

fn is_hidden(el: ElementRef<'_>) -> bool {
    let val = el.value();
    if val.attr("hidden").is_some() || val.attr("aria-hidden") == Some("true") {
        return true;
    }
    if let Some(style) = val.attr("style") {
        let compact: String = style
            .chars()
            .filter(|c| !c.is_whitespace())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        if compact.contains("display:none") || compact.contains("visibility:hidden") {
            return true;
        }
    }
    false
}

fn clean_inline(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn collapse_ws_preserve_edges(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(ch);
            prev_ws = false;
        }
    }
    out
}

fn collapse_newlines_to_space(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_nl = false;
    for ch in s.chars() {
        if ch == '\n' || ch == '\r' {
            if !prev_nl {
                out.push(' ');
                prev_nl = true;
            }
        } else {
            out.push(ch);
            prev_nl = false;
        }
    }
    out.trim().to_string()
}

fn normalize_newlines(s: &str) -> String {
    let lf = s.replace("\r\n", "\n");
    let stripped_lines: Vec<&str> = lf
        .split('\n')
        .map(|line| line.trim_end_matches([' ', '\t']))
        .collect();
    let joined = stripped_lines.join("\n");
    let mut out = String::with_capacity(joined.len());
    let mut nl_run = 0usize;
    for ch in joined.chars() {
        if ch == '\n' {
            nl_run += 1;
            if nl_run <= 2 {
                out.push('\n');
            }
        } else {
            nl_run = 0;
            out.push(ch);
        }
    }
    out.trim().to_string()
}
