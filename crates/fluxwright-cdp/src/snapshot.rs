//! Compact accessibility tree snapshots with short `@eN` element refs, and
//! clean Markdown serialization options.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

/// Controls how [`crate::CdpPage::aria_snapshot`] shapes the returned tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOptions {
    /// Emit only interactive / focusable controls (flattened list with refs).
    pub interactive_only: bool,
    /// Omit unnamed generic/layout wrapper roles while still traversing their children.
    pub compact: bool,
    /// Maximum visual tree depth to emit (`None` = unlimited).
    pub max_depth: Option<usize>,
    /// Maximum number of emitted nodes before truncating (`Default`: 1500).
    pub max_nodes: usize,
    /// Return a compact line-level diff against the previous snapshot on the page (`Default`: `false`).
    pub diff: bool,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            interactive_only: false,
            compact: true,
            max_depth: None,
            max_nodes: 1500,
            diff: false,
        }
    }
}

/// An addressable DOM element from the latest accessibility snapshot (`@e1`, `@e2`, ...).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AriaRef {
    pub id: String,
    pub role: String,
    pub name: String,
    #[serde(skip_serializing)]
    pub backend_node_id: i64,
}

/// Formatted compact accessibility tree paired with resolved element refs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AriaSnapshot {
    pub tree: String,
    pub refs: Vec<AriaRef>,
    pub truncated: bool,
    pub is_diff: bool,
    pub changed: bool,
}

/// Controls how [`crate::CdpPage::markdown`] serializes a page or element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownOptions {
    /// Optional CSS/locator selector to serialize instead of the whole document.
    pub selector: Option<String>,
    /// Prefer `<main>`, `<article>`, or `[role="main"]` when `selector` is not given.
    pub main_content_only: bool,
    /// Render `[label](url)` links (default `true`).
    pub include_links: bool,
    /// Render `![alt](src)` images (default `false`).
    pub include_images: bool,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            selector: None,
            main_content_only: false,
            include_links: true,
            include_images: false,
        }
    }
}

/// Extracts the normalized ref key (`"e12"`) when a selector begins with `@e12` or `ref=e12`.
pub(crate) fn extract_ax_ref(query: &str) -> Option<String> {
    let first_part = query.split(">>").next()?.trim();
    let candidate = first_part
        .strip_prefix('@')
        .or_else(|| first_part.strip_prefix("ref="))?;
    let rest = candidate.strip_prefix('e')?;
    if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()) {
        Some(candidate.to_string())
    } else {
        None
    }
}

pub(crate) fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "combobox"
            | "listbox"
            | "option"
            | "checkbox"
            | "radio"
            | "switch"
            | "slider"
            | "spinbutton"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "treeitem"
            | "summary"
    )
}

fn is_wrapper_role(role: &str) -> bool {
    matches!(
        role,
        "generic"
            | "none"
            | "presentation"
            | "InlineTextBox"
            | "LayoutTable"
            | "LayoutTableRow"
            | "LayoutTableCell"
            | "SectionFooter"
            | "SectionHeader"
            | "Paragraph"
    )
}

fn is_non_ref_role(role: &str) -> bool {
    matches!(role, "RootWebArea" | "WebArea" | "StaticText" | "LineBreak")
}

/// Converts raw `Accessibility.getFullAXTree` `nodes` into a compact LLM-friendly tree + `@eN` refs.
pub fn build_aria_snapshot(raw_nodes: &[Value], opts: &SnapshotOptions) -> AriaSnapshot {
    if raw_nodes.is_empty() || opts.max_nodes == 0 {
        return AriaSnapshot {
            tree: String::new(),
            refs: Vec::new(),
            truncated: !raw_nodes.is_empty() && opts.max_nodes == 0,
            is_diff: false,
            changed: true,
        };
    }

    let mut by_id: HashMap<&str, usize> = HashMap::with_capacity(raw_nodes.len());
    let mut referenced_children: HashSet<&str> = HashSet::new();
    for (idx, node) in raw_nodes.iter().enumerate() {
        if let Some(nid) = node["nodeId"].as_str() {
            by_id.insert(nid, idx);
        }
        if let Some(children) = node["childIds"].as_array() {
            for cid in children.iter().filter_map(Value::as_str) {
                referenced_children.insert(cid);
            }
        }
    }

    let root_idx = raw_nodes
        .iter()
        .position(|n| n["role"]["value"].as_str() == Some("RootWebArea"))
        .or_else(|| {
            raw_nodes.iter().position(|n| {
                n["parentId"].is_null()
                    && n["nodeId"]
                        .as_str()
                        .is_some_and(|id| !referenced_children.contains(id))
            })
        })
        .unwrap_or(0);

    let mut ctx = WalkCtx {
        raw_nodes,
        by_id: &by_id,
        opts,
        visited: HashSet::with_capacity(raw_nodes.len()),
        lines: Vec::new(),
        refs: Vec::new(),
        next_ref: 1,
        truncated: false,
    };

    if let Some(root) = raw_nodes.get(root_idx) {
        ctx.visit(root, 0, None);
    }

    AriaSnapshot {
        tree: ctx.lines.join("\n"),
        refs: ctx.refs,
        truncated: ctx.truncated,
        is_diff: false,
        changed: true,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// Computes a compact line-level unified diff between two formatted ARIA snapshot trees.
pub fn diff_snapshot_trees(prev: &str, next: &str) -> String {
    if prev == next {
        return "(no changes)".into();
    }
    let old_lines: Vec<&str> = if prev.is_empty() {
        Vec::new()
    } else {
        prev.lines().collect()
    };
    let new_lines: Vec<&str> = if next.is_empty() {
        Vec::new()
    } else {
        next.lines().collect()
    };

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }

    let mut suffix = 0usize;
    while suffix < (old_lines.len() - prefix)
        && suffix < (new_lines.len() - prefix)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }

    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];

    let mut ops = Vec::with_capacity(old_lines.len() + new_lines.len());
    for i in 0..prefix {
        ops.push(DiffOp::Equal(i, i));
    }

    let n = old_mid.len();
    let m = new_mid.len();
    if n == 0 {
        for j in 0..m {
            ops.push(DiffOp::Insert(prefix + j));
        }
    } else if m == 0 {
        for i in 0..n {
            ops.push(DiffOp::Delete(prefix + i));
        }
    } else if (n + 1).saturating_mul(m + 1) <= 2_500_000 {
        let cols = m + 1;
        let mut dp = vec![0u32; (n + 1) * cols];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i * cols + j] = if old_mid[i] == new_mid[j] {
                    dp[(i + 1) * cols + (j + 1)] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0usize, 0usize);
        while i < n && j < m {
            if old_mid[i] == new_mid[j] {
                ops.push(DiffOp::Equal(prefix + i, prefix + j));
                i += 1;
                j += 1;
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
                ops.push(DiffOp::Delete(prefix + i));
                i += 1;
            } else {
                ops.push(DiffOp::Insert(prefix + j));
                j += 1;
            }
        }
        while i < n {
            ops.push(DiffOp::Delete(prefix + i));
            i += 1;
        }
        while j < m {
            ops.push(DiffOp::Insert(prefix + j));
            j += 1;
        }
    } else {
        for i in 0..n {
            ops.push(DiffOp::Delete(prefix + i));
        }
        for j in 0..m {
            ops.push(DiffOp::Insert(prefix + j));
        }
    }

    for k in 0..suffix {
        ops.push(DiffOp::Equal(
            old_lines.len() - suffix + k,
            new_lines.len() - suffix + k,
        ));
    }

    const CONTEXT: usize = 1;
    let mut raw_hunks: Vec<(usize, usize)> = Vec::new();
    let mut idx = 0usize;
    while idx < ops.len() {
        if matches!(ops[idx], DiffOp::Equal(_, _)) {
            idx += 1;
            continue;
        }
        let mut end = idx + 1;
        while end < ops.len() && !matches!(ops[end], DiffOp::Equal(_, _)) {
            end += 1;
        }
        let mut h_start = idx.saturating_sub(CONTEXT);
        if h_start == 1 {
            // Avoid skipping a single leading line only to replace it with a hunk header.
            h_start = 0;
        }
        let h_end = (end + CONTEXT).min(ops.len());
        if let Some(last) = raw_hunks.last_mut() {
            if h_start <= last.1 + 1 {
                last.1 = last.1.max(h_end);
            } else {
                raw_hunks.push((h_start, h_end));
            }
        } else {
            raw_hunks.push((h_start, h_end));
        }
        idx = end;
    }

    if raw_hunks.is_empty() {
        return "(no changes)".into();
    }

    let mut out = Vec::new();
    for (h_start, h_end) in raw_hunks {
        if h_start > 0 {
            let (mut old_first, mut new_first) = (None, None);
            let (mut old_cnt, mut new_cnt) = (0usize, 0usize);
            for op in &ops[h_start..h_end] {
                match *op {
                    DiffOp::Equal(oi, ni) => {
                        old_first.get_or_insert(oi + 1);
                        new_first.get_or_insert(ni + 1);
                        old_cnt += 1;
                        new_cnt += 1;
                    }
                    DiffOp::Delete(oi) => {
                        old_first.get_or_insert(oi + 1);
                        old_cnt += 1;
                    }
                    DiffOp::Insert(ni) => {
                        new_first.get_or_insert(ni + 1);
                        new_cnt += 1;
                    }
                }
            }
            let o_start = old_first.unwrap_or(1);
            let n_start = new_first.unwrap_or(1);
            out.push(format!("@@ -{o_start},{old_cnt} +{n_start},{new_cnt} @@"));
        }
        for op in &ops[h_start..h_end] {
            match *op {
                DiffOp::Equal(oi, _) => out.push(format!("  {}", old_lines[oi])),
                DiffOp::Delete(oi) => out.push(format!("- {}", old_lines[oi])),
                DiffOp::Insert(ni) => out.push(format!("+ {}", new_lines[ni])),
            }
        }
    }
    out.join("\n")
}

struct WalkCtx<'a> {
    raw_nodes: &'a [Value],
    by_id: &'a HashMap<&'a str, usize>,
    opts: &'a SnapshotOptions,
    visited: HashSet<&'a str>,
    lines: Vec<String>,
    refs: Vec<AriaRef>,
    next_ref: usize,
    truncated: bool,
}

impl<'a> WalkCtx<'a> {
    fn visit(&mut self, node: &'a Value, depth: usize, parent_name: Option<&str>) {
        if self.truncated {
            return;
        }
        let Some(nid) = node["nodeId"].as_str() else {
            return;
        };
        if !self.visited.insert(nid) {
            return;
        }

        let role = node["role"]["value"].as_str().unwrap_or("").trim();
        let ignored = node["ignored"].as_bool().unwrap_or(false);
        let name = norm_str(node["name"]["value"].as_str().unwrap_or(""));

        let child_ids: Vec<&'a str> = node["childIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();

        if ignored || role.is_empty() {
            for cid in child_ids {
                if let Some(&idx) = self.by_id.get(cid) {
                    self.visit(&self.raw_nodes[idx], depth, parent_name);
                }
            }
            return;
        }

        if role == "LineBreak" {
            return;
        }
        if role == "StaticText" && (name.is_empty() || parent_name == Some(name.as_str())) {
            return;
        }

        let props = extract_props(node);
        let focusable = props.iter().any(|p| p == "focusable");
        let interactive = is_interactive_role(role) || (focusable && !is_non_ref_role(role));

        // Skip unnamed wrapper roles cleanly while preserving descendants at the same visual depth.
        if self.opts.compact && is_wrapper_role(role) && name.is_empty() && !interactive {
            for cid in child_ids {
                if let Some(&idx) = self.by_id.get(cid) {
                    self.visit(&self.raw_nodes[idx], depth, parent_name);
                }
            }
            return;
        }

        if let Some(max_d) = self.opts.max_depth {
            if depth > max_d {
                return;
            }
        }

        let emit = if self.opts.interactive_only {
            interactive
        } else {
            !(role == "generic" && name.is_empty() && props.is_empty() && node["value"]["value"].is_null())
        };

        let mut next_depth = depth;
        if emit {
            if self.lines.len() >= self.opts.max_nodes {
                self.truncated = true;
                return;
            }
            let ref_id = if !is_non_ref_role(role) {
                node["backendDOMNodeId"].as_i64().map(|backend_id| {
                    let id = format!("e{}", self.next_ref);
                    self.next_ref += 1;
                    self.refs.push(AriaRef {
                        id: id.clone(),
                        role: role.to_string(),
                        name: name.clone(),
                        backend_node_id: backend_id,
                    });
                    id
                })
            } else {
                None
            };

            let indent = if self.opts.interactive_only {
                0
            } else {
                depth * 2
            };
            let mut line = format!("{:indent$}- {role}", "", indent = indent);
            if !name.is_empty() {
                line.push(' ');
                line.push_str(&quote_truncated(&name, 160));
            }
            if let Some(ref id) = ref_id {
                line.push_str(&format!(" [ref={id}]"));
            }
            for prop in &props {
                if prop != "focusable" {
                    line.push_str(&format!(" [{prop}]"));
                }
            }
            if let Some(val_str) = node["value"]["value"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                line.push_str(" value=");
                line.push_str(&quote_truncated(val_str, 120));
            }
            self.lines.push(line);
            if !self.opts.interactive_only {
                next_depth = depth + 1;
            }
        }

        let child_parent_name = if !name.is_empty() {
            Some(name.as_str())
        } else {
            parent_name
        };
        for cid in child_ids {
            if let Some(&idx) = self.by_id.get(cid) {
                self.visit(&self.raw_nodes[idx], next_depth, child_parent_name);
            }
            if self.truncated {
                return;
            }
        }
    }
}

fn extract_props(node: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(arr) = node["properties"].as_array() else {
        return out;
    };
    for prop in arr {
        let Some(name) = prop["name"].as_str() else {
            continue;
        };
        let val = &prop["value"]["value"];
        match name {
            "level" => {
                if let Some(n) = val.as_i64() {
                    out.push(format!("level={n}"));
                }
            }
            "disabled" | "expanded" | "selected" | "required" | "readonly" | "focusable" => {
                if val.as_bool() == Some(true) {
                    out.push(name.to_string());
                }
            }
            "checked" | "pressed" => match val.as_str() {
                Some("true") => out.push(name.to_string()),
                Some("mixed") => out.push(format!("{name}=mixed")),
                _ if val.as_bool() == Some(true) => out.push(name.to_string()),
                _ => {}
            },
            "invalid" => {
                if val.as_bool() == Some(true) || matches!(val.as_str(), Some("true" | "grammar" | "spelling")) {
                    out.push("invalid".into());
                }
            }
            "url" => {
                if let Some(u) = val.as_str().map(str::trim).filter(|s| !s.is_empty() && !s.starts_with("data:")) {
                    let clipped = truncate_chars(u, 120);
                    out.push(format!("url={clipped}"));
                }
            }
            _ => {}
        }
    }
    out
}

fn norm_str(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let cut = s.floor_char_boundary(max_bytes);
    format!("{}...", &s[..cut])
}

fn quote_truncated(s: &str, max_bytes: usize) -> String {
    serde_json::to_string(&truncate_chars(s, max_bytes)).unwrap_or_else(|_| "\"\"".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_only_canonical_ax_refs() {
        assert_eq!(extract_ax_ref("@e1"), Some("e1".into()));
        assert_eq!(extract_ax_ref("  ref=e42  "), Some("e42".into()));
        assert_eq!(extract_ax_ref("@e7 >> nth=0"), Some("e7".into()));
        assert_eq!(extract_ax_ref("ref=e9 >> has-text=\"Go\""), Some("e9".into()));

        assert_eq!(extract_ax_ref("#btn"), None);
        assert_eq!(extract_ax_ref("@foo"), None);
        assert_eq!(extract_ax_ref("@e"), None);
        assert_eq!(extract_ax_ref("@e1x"), None);
        assert_eq!(extract_ax_ref("text=@e1"), None);
    }

    #[test]
    fn builds_compact_tree_flattens_ignored_wrappers_and_dedups_static_text() {
        let nodes = vec![
            json!({
                "nodeId": "1",
                "role": { "value": "RootWebArea" },
                "name": { "value": "Store" },
                "backendDOMNodeId": 100,
                "childIds": ["2", "3"]
            }),
            json!({
                "nodeId": "2",
                "ignored": true,
                "role": { "value": "generic" },
                "childIds": ["4", "5"]
            }),
            json!({
                "nodeId": "3",
                "role": { "value": "generic" },
                "childIds": ["6"]
            }),
            json!({
                "nodeId": "4",
                "role": { "value": "heading" },
                "name": { "value": "Welcome" },
                "backendDOMNodeId": 101,
                "properties": [{ "name": "level", "value": { "value": 1 } }],
                "childIds": ["4a"]
            }),
            json!({
                "nodeId": "4a",
                "role": { "value": "StaticText" },
                "name": { "value": "Welcome" }
            }),
            json!({
                "nodeId": "5",
                "role": { "value": "button" },
                "name": { "value": "Checkout" },
                "backendDOMNodeId": 102,
                "properties": [{ "name": "disabled", "value": { "value": true } }],
                "childIds": ["5a"]
            }),
            json!({
                "nodeId": "5a",
                "role": { "value": "StaticText" },
                "name": { "value": "Checkout" }
            }),
            json!({
                "nodeId": "6",
                "role": { "value": "textbox" },
                "name": { "value": "Email" },
                "backendDOMNodeId": 103,
                "value": { "value": "ada@example.com" }
            }),
        ];

        let snap = build_aria_snapshot(&nodes, &SnapshotOptions::default());
        assert!(!snap.truncated);
        assert_eq!(snap.refs.len(), 3);
        assert_eq!(snap.refs[0].id, "e1");
        assert_eq!(snap.refs[1].backend_node_id, 102);
        assert_eq!(snap.refs[2].role, "textbox");

        let expected = "\
- RootWebArea \"Store\"
  - heading \"Welcome\" [ref=e1] [level=1]
  - button \"Checkout\" [ref=e2] [disabled]
  - textbox \"Email\" [ref=e3] value=\"ada@example.com\"";
        assert_eq!(snap.tree, expected);
    }

    #[test]
    fn interactive_only_and_max_nodes_filter_cleanly() {
        let nodes = vec![
            json!({
                "nodeId": "1",
                "role": { "value": "RootWebArea" },
                "name": { "value": "Page" },
                "childIds": ["2", "3", "4"]
            }),
            json!({
                "nodeId": "2",
                "role": { "value": "heading" },
                "name": { "value": "Title" },
                "backendDOMNodeId": 10
            }),
            json!({
                "nodeId": "3",
                "role": { "value": "link" },
                "name": { "value": "Docs" },
                "backendDOMNodeId": 11,
                "properties": [{ "name": "url", "value": { "value": "https://example.com/docs" } }]
            }),
            json!({
                "nodeId": "4",
                "role": { "value": "checkbox" },
                "name": { "value": "Accept" },
                "backendDOMNodeId": 12,
                "properties": [{ "name": "checked", "value": { "value": "mixed" } }]
            }),
        ];

        let interactive = build_aria_snapshot(
            &nodes,
            &SnapshotOptions {
                interactive_only: true,
                ..Default::default()
            },
        );
        assert_eq!(
            interactive.tree,
            "- link \"Docs\" [ref=e1] [url=https://example.com/docs]\n- checkbox \"Accept\" [ref=e2] [checked=mixed]"
        );
        assert_eq!(interactive.refs.len(), 2);

        let capped = build_aria_snapshot(
            &nodes,
            &SnapshotOptions {
                max_nodes: 2,
                ..Default::default()
            },
        );
        assert!(capped.truncated);
        assert!(!capped.is_diff);
        assert!(capped.changed);
        assert_eq!(capped.lines_count(), 2);
    }

    #[test]
    fn diff_snapshot_trees_reports_no_changes_compact_hunks_and_edges() {
        let base = "\
- RootWebArea \"App\"
  - heading \"Title\" [ref=e1] [level=1]
  - button \"Save\" [ref=e2]
  - link \"Docs\" [ref=e3]";
        assert_eq!(diff_snapshot_trees(base, base), "(no changes)");

        let changed = "\
- RootWebArea \"App\"
  - heading \"Title\" [ref=e1] [level=1]
  - button \"Saved\" [ref=e2]
  - status \"Done\"
  - link \"Docs\" [ref=e3]";
        let diff = diff_snapshot_trees(base, changed);
        assert_eq!(
            diff,
            "\
  - RootWebArea \"App\"
    - heading \"Title\" [ref=e1] [level=1]
-   - button \"Save\" [ref=e2]
+   - button \"Saved\" [ref=e2]
+   - status \"Done\"
    - link \"Docs\" [ref=e3]"
        );

        let many_old: Vec<String> = (1..=30).map(|i| format!("  - item {i}")).collect();
        let mut many_new = many_old.clone();
        many_new[14] = "  - item 15 [updated]".into();
        let hunk_diff = diff_snapshot_trees(&many_old.join("\n"), &many_new.join("\n"));
        assert!(hunk_diff.contains("@@ -14,3 +14,3 @@"), "{hunk_diff}");
        assert!(hunk_diff.contains("-   - item 15"), "{hunk_diff}");
        assert!(hunk_diff.contains("+   - item 15 [updated]"), "{hunk_diff}");
        assert!(!hunk_diff.contains("item 1\n"), "{hunk_diff}");
        assert!(!hunk_diff.contains("item 30"), "{hunk_diff}");

        assert_eq!(diff_snapshot_trees("", "- button \"Go\""), "+ - button \"Go\"");
        assert_eq!(diff_snapshot_trees("- button \"Go\"", ""), "- - button \"Go\"");
    }

    trait LineCount {
        fn lines_count(&self) -> usize;
    }
    impl LineCount for AriaSnapshot {
        fn lines_count(&self) -> usize {
            if self.tree.is_empty() { 0 } else { self.tree.lines().count() }
        }
    }
}
