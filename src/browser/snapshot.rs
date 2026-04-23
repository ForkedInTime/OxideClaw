//! Accessibility tree extraction and @ref numbering.
//!
//! Uses Chrome's Accessibility.getFullAXTree CDP command. Filters by
//! interactive/content roles, assigns integer refs (@e1, @e2, ...).

use std::collections::HashMap;

/// ARIA roles that are interactive (buttons, links, inputs, etc.)
const INTERACTIVE_ROLES: &[&str] = &[
    "button", "link", "textbox", "searchbox", "checkbox", "radio",
    "combobox", "listbox", "option", "menuitem", "menuitemcheckbox",
    "menuitemradio", "tab", "switch", "slider", "spinbutton",
    "scrollbar", "treeitem", "gridcell", "columnheader", "rowheader",
];

/// ARIA roles that carry content worth showing
const CONTENT_ROLES: &[&str] = &[
    "heading", "img", "image", "paragraph", "list", "listitem",
    "table", "row", "cell", "navigation", "banner", "main",
    "complementary", "contentinfo", "form", "region", "alert",
    "status", "dialog",
];

/// Roles to always skip
const SKIP_ROLES: &[&str] = &[
    "none", "presentation", "generic", "RootWebArea", "InlineTextBox",
    "LineBreak",
];

/// Parse CDP Accessibility.getFullAXTree node array into a text representation,
/// a ref map (ref -> backendDOMNodeId), and a parallel name map (ref -> label).
///
/// The name map only contains entries where the node has a non-empty accessible
/// name — it's used by the approval gate to pattern-match real button labels
/// against destructive-action regexes.
///
/// Note: we store `backendDOMNodeId` (DOM-tree backend ID), NOT `nodeId`
/// (AX-tree-local ID). Downstream actions (click, fill, etc.) pass this
/// to CDP DOM.resolveNode / DOM.getBoxModel, which require the backend ID.
/// Nodes without a `backendDOMNodeId` (synthetic AX nodes) are skipped.
pub fn parse_ax_nodes_full(
    nodes: &serde_json::Value,
) -> (String, HashMap<String, i64>, HashMap<String, String>) {
    let mut ref_map = HashMap::new();
    let mut name_map = HashMap::new();
    let mut lines = Vec::new();
    let mut ref_counter = 1u32;

    let arr = match nodes.as_array() {
        Some(a) => a,
        None => return (String::new(), ref_map, name_map),
    };

    for node in arr {
        let role = node["role"]["value"].as_str().unwrap_or("");
        let name = node["name"]["value"].as_str().unwrap_or("").trim();
        // Prefer backendDOMNodeId (an integer). Fall back to parsing nodeId
        // from a string for test fixtures / older CDP responses.
        let backend_id = node["backendDOMNodeId"]
            .as_i64()
            .or_else(|| node["nodeId"].as_str().and_then(|s| s.parse().ok()));

        if SKIP_ROLES.contains(&role) {
            continue;
        }

        let is_interactive = INTERACTIVE_ROLES.contains(&role);
        let is_content = CONTENT_ROLES.contains(&role);
        if !is_interactive && !is_content {
            continue;
        }
        if name.is_empty() && !is_interactive {
            continue;
        }

        // Skip nodes without any usable DOM reference.
        let Some(node_id) = backend_id else { continue };

        let ref_str = format!("@e{ref_counter}");
        ref_map.insert(ref_str.clone(), node_id);
        if !name.is_empty() {
            name_map.insert(ref_str.clone(), name.to_string());
        }

        let display_name = if name.is_empty() {
            format!("{ref_str} [{role}]")
        } else {
            format!("{ref_str} [{role}] \"{name}\"")
        };
        lines.push(display_name);
        ref_counter += 1;
    }

    (lines.join("\n"), ref_map, name_map)
}

/// Back-compat wrapper: returns only the tree + ref map. Used by existing tests.
#[allow(dead_code)]
pub fn parse_ax_nodes(nodes: &serde_json::Value) -> (String, HashMap<String, i64>) {
    let (tree, refs, _) = parse_ax_nodes_full(nodes);
    (tree, refs)
}

/// Take a full accessibility snapshot via CDP, including the name map.
pub async fn take_snapshot(
    client: &super::cdp::CdpClient,
) -> anyhow::Result<(String, HashMap<String, i64>, HashMap<String, String>)> {
    let result = client
        .send("Accessibility.getFullAXTree", serde_json::json!({}))
        .await?;
    let nodes = &result["nodes"];
    Ok(parse_ax_nodes_full(nodes))
}
