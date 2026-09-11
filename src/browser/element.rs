//! Element scoring and filtering (ClickableElementDetector).
//!
//! Scores elements by ARIA role, interactivity signals, and label presence.
//! Higher scores = more likely to be the element the user/AI wants.
//!
//! Not yet consumed — `snapshot.rs` currently includes every visible
//! interactive node unscored. Kept for the planned "top-N clickable
//! elements" snapshot mode.

/// Score an element based on its properties. Higher = more interactive/important.
#[allow(dead_code)]
pub fn score_element(
    role: &str,
    has_click_listener: bool,
    has_label: bool,
    is_focusable: bool,
) -> u32 {
    let mut score = 0u32;

    score += match role {
        "button" | "link" => 10,
        "textbox" | "searchbox" | "combobox" => 9,
        "checkbox" | "radio" | "switch" => 8,
        "menuitem" | "menuitemcheckbox" | "menuitemradio" => 7,
        "option" | "tab" | "treeitem" => 6,
        "slider" | "spinbutton" => 5,
        "heading" => 4,
        "img" | "image" => 3,
        "listitem" | "cell" | "row" => 2,
        _ => 1,
    };

    if has_click_listener {
        score += 5;
    }
    if has_label {
        score += 3;
    }
    if is_focusable {
        score += 2;
    }

    score
}
