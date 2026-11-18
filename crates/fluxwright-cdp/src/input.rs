//! Rich pointer, keyboard, form, file upload, and touch helpers over CDP `Input` / `DOM`.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{json, Value};

use crate::connection::Connection;
use crate::error::{Error, Result};

/// Mouse button for `click`, `dblclick`, and `page.mouse`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn as_cdp_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    pub(crate) fn bit(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
            Self::Middle => 4,
        }
    }
}

impl FromStr for MouseButton {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "left" => Ok(Self::Left),
            "right" => Ok(Self::Right),
            "middle" => Ok(Self::Middle),
            _ => Err(Error::Other(format!(
                "button must be left, right or middle, not {s:?}"
            ))),
        }
    }
}

/// CDP modifier bit for Playwright's `Alt`, `Control`, `Meta`, `Shift`, and `ControlOrMeta`.
pub fn modifier_bit(name: &str) -> Result<u8> {
    match name {
        "Alt" => Ok(1),
        "Control" => Ok(2),
        "Meta" => Ok(4),
        "Shift" => Ok(8),
        "ControlOrMeta" => Ok(if cfg!(target_os = "macos") { 4 } else { 2 }),
        _ => Err(Error::Other(format!(
            "modifier must be Alt, Control, Meta, Shift or ControlOrMeta, not {name:?}"
        ))),
    }
}

/// Converts a list of modifier names into CDP's bitmask.
pub fn parse_modifiers(mods: &[String]) -> Result<u8> {
    let mut mask = 0u8;
    for m in mods {
        mask |= modifier_bit(m)?;
    }
    Ok(mask)
}

/// Options for `click` and `dblclick`, matching Playwright.
#[derive(Debug, Clone, Default)]
pub struct ClickOptions {
    pub button: MouseButton,
    /// Number of clicks (1 by default; `dblclick` sets 2).
    pub click_count: u32,
    /// Pause between `mousePressed` and `mouseReleased`.
    pub delay: Option<Duration>,
    /// Offset from the top-left of the element's bounding box; defaults to its centre.
    pub position: Option<(f64, f64)>,
    /// Bitmask built with [`parse_modifiers`].
    pub modifiers: u8,
    /// Skip visibility, enabled, stability, and hit-target actionability checks.
    pub force: bool,
    /// Run actionability checks without dispatching the click.
    pub trial: bool,
}

/// Options for `hover` and `tap`, matching Playwright.
#[derive(Debug, Clone, Default)]
pub struct HoverOptions {
    pub position: Option<(f64, f64)>,
    pub modifiers: u8,
    pub force: bool,
    pub trial: bool,
}

/// Options for `check`, `uncheck`, and `set_checked`.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    pub position: Option<(f64, f64)>,
    pub force: bool,
    pub trial: bool,
}

/// One selector criterion for `select_option`, matching Playwright.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectEntry {
    Value(String),
    Label(String),
    Index(usize),
}

impl SelectEntry {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Self::Value(v) => json!({ "value": v }),
            Self::Label(l) => json!({ "label": l }),
            Self::Index(i) => json!({ "index": i }),
        }
    }
}

/// In-memory file payload for `set_input_files`, matching Playwright's `{ name, mimeType, buffer }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePayload {
    pub name: String,
    pub mime_type: String,
    pub buffer: Vec<u8>,
}

/// Files to assign to `<input type="file">` or a `FileChooser`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileInput {
    Paths(Vec<PathBuf>),
    Payloads(Vec<FilePayload>),
}

impl FileInput {
    pub fn len(&self) -> usize {
        match self {
            Self::Paths(p) => p.len(),
            Self::Payloads(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub(crate) async fn canonicalize_local_paths(paths: &[PathBuf]) -> Result<Vec<String>> {
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        let meta = tokio::fs::metadata(p).await.map_err(|e| {
            Error::Other(format!("set_input_files {}: {e}", p.display()))
        })?;
        if !meta.is_file() {
            return Err(Error::Other(format!(
                "set_input_files {}: not a regular file",
                p.display()
            )));
        }
        let canon = tokio::fs::canonicalize(p).await.map_err(|e| {
            Error::Other(format!("set_input_files {}: {e}", p.display()))
        })?;
        out.push(canon.to_string_lossy().into_owned());
    }
    Ok(out)
}

pub(crate) async fn encode_file_payloads(input: &FileInput) -> Result<Vec<Value>> {
    use base64::Engine;
    match input {
        FileInput::Payloads(list) => Ok(list
            .iter()
            .map(|f| {
                json!({
                    "name": f.name,
                    "mimeType": f.mime_type,
                    "base64": base64::engine::general_purpose::STANDARD.encode(&f.buffer),
                })
            })
            .collect()),
        FileInput::Paths(paths) => {
            let mut out = Vec::with_capacity(paths.len());
            for p in paths {
                let bytes = tokio::fs::read(p).await.map_err(|e| {
                    Error::Other(format!("set_input_files {}: {e}", p.display()))
                })?;
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "file".into());
                out.push(json!({
                    "name": name,
                    "mimeType": "application/octet-stream",
                    "base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                }));
            }
            Ok(out)
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct MouseState {
    pub x: f64,
    pub y: f64,
    pub buttons: u8,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct KeyboardState {
    pub modifiers: u8,
}

pub(crate) struct KeySpec {
    pub key: String,
    pub code: &'static str,
    pub vk: u32,
    pub text: Option<String>,
    /// True if matching a shifted symbol on a US layout implies Shift when no modifiers are held.
    pub shifted: bool,
    pub is_modifier: Option<u8>,
}

pub(crate) fn parse_chord(combo: &str) -> Result<(Vec<&'static str>, &str)> {
    if combo.is_empty() {
        return Err(Error::Other("key must not be empty".into()));
    }
    let mut mods = Vec::new();
    let mut rest = combo;
    while let Some(pos) = rest.find('+') {
        if pos == 0 && rest.len() == 1 {
            break;
        }
        let head = &rest[..pos];
        if head.is_empty() {
            return Err(Error::Other(format!("invalid key combination {combo:?}")));
        }
        let canon = match head {
            "Alt" => "Alt",
            "Control" => "Control",
            "Meta" => "Meta",
            "Shift" => "Shift",
            "ControlOrMeta" => {
                if cfg!(target_os = "macos") {
                    "Meta"
                } else {
                    "Control"
                }
            }
            _ => {
                return Err(Error::Other(format!(
                    "unknown modifier {head:?} in key combination {combo:?}"
                )));
            }
        };
        mods.push(canon);
        rest = &rest[pos + 1..];
    }
    if rest.is_empty() {
        return Err(Error::Other(format!("missing key in combination {combo:?}")));
    }
    Ok((mods, rest))
}

pub(crate) fn resolve_key(name: &str) -> Result<KeySpec> {
    let spec = |key: &str, code: &'static str, vk: u32, text: Option<&str>| KeySpec {
        key: key.into(),
        code,
        vk,
        text: text.map(Into::into),
        shifted: false,
        is_modifier: None,
    };
    let named = match name {
        "Enter" | "NumpadEnter" => Some(KeySpec {
            key: "Enter".into(),
            code: if name == "NumpadEnter" { "NumpadEnter" } else { "Enter" },
            vk: 13,
            text: Some("\r".into()),
            shifted: false,
            is_modifier: None,
        }),
        "Tab" => Some(spec("Tab", "Tab", 9, None)),
        "Backspace" => Some(spec("Backspace", "Backspace", 8, None)),
        "Delete" => Some(spec("Delete", "Delete", 46, None)),
        "Escape" => Some(spec("Escape", "Escape", 27, None)),
        "Space" | " " => Some(spec(" ", "Space", 32, Some(" "))),
        "ArrowUp" => Some(spec("ArrowUp", "ArrowUp", 38, None)),
        "ArrowDown" => Some(spec("ArrowDown", "ArrowDown", 40, None)),
        "ArrowLeft" => Some(spec("ArrowLeft", "ArrowLeft", 37, None)),
        "ArrowRight" => Some(spec("ArrowRight", "ArrowRight", 39, None)),
        "Home" => Some(spec("Home", "Home", 36, None)),
        "End" => Some(spec("End", "End", 35, None)),
        "PageUp" => Some(spec("PageUp", "PageUp", 33, None)),
        "PageDown" => Some(spec("PageDown", "PageDown", 34, None)),
        "Insert" => Some(spec("Insert", "Insert", 45, None)),
        "CapsLock" => Some(spec("CapsLock", "CapsLock", 20, None)),
        "F1" => Some(spec("F1", "F1", 112, None)),
        "F2" => Some(spec("F2", "F2", 113, None)),
        "F3" => Some(spec("F3", "F3", 114, None)),
        "F4" => Some(spec("F4", "F4", 115, None)),
        "F5" => Some(spec("F5", "F5", 116, None)),
        "F6" => Some(spec("F6", "F6", 117, None)),
        "F7" => Some(spec("F7", "F7", 118, None)),
        "F8" => Some(spec("F8", "F8", 119, None)),
        "F9" => Some(spec("F9", "F9", 120, None)),
        "F10" => Some(spec("F10", "F10", 121, None)),
        "F11" => Some(spec("F11", "F11", 122, None)),
        "F12" => Some(spec("F12", "F12", 123, None)),
        "Shift" | "ShiftLeft" | "ShiftRight" => Some(KeySpec {
            key: "Shift".into(),
            code: if name == "ShiftRight" { "ShiftRight" } else { "ShiftLeft" },
            vk: 16,
            text: None,
            shifted: false,
            is_modifier: Some(8),
        }),
        "Control" | "ControlLeft" | "ControlRight" => Some(KeySpec {
            key: "Control".into(),
            code: if name == "ControlRight" { "ControlRight" } else { "ControlLeft" },
            vk: 17,
            text: None,
            shifted: false,
            is_modifier: Some(2),
        }),
        "Alt" | "AltLeft" | "AltRight" => Some(KeySpec {
            key: "Alt".into(),
            code: if name == "AltRight" { "AltRight" } else { "AltLeft" },
            vk: 18,
            text: None,
            shifted: false,
            is_modifier: Some(1),
        }),
        "Meta" | "MetaLeft" | "MetaRight" => Some(KeySpec {
            key: "Meta".into(),
            code: if name == "MetaRight" { "MetaRight" } else { "MetaLeft" },
            vk: 91,
            text: None,
            shifted: false,
            is_modifier: Some(4),
        }),
        "ControlOrMeta" => {
            if cfg!(target_os = "macos") {
                Some(KeySpec {
                    key: "Meta".into(),
                    code: "MetaLeft",
                    vk: 91,
                    text: None,
                    shifted: false,
                    is_modifier: Some(4),
                })
            } else {
                Some(KeySpec {
                    key: "Control".into(),
                    code: "ControlLeft",
                    vk: 17,
                    text: None,
                    shifted: false,
                    is_modifier: Some(2),
                })
            }
        }
        _ => None,
    };
    if let Some(s) = named {
        return Ok(s);
    }
    if let Some(rest) = name.strip_prefix("Key").filter(|s| s.len() == 1) {
        let ch = rest.chars().next().unwrap().to_ascii_uppercase();
        if ch.is_ascii_uppercase() {
            return Ok(KeySpec {
                key: ch.to_ascii_lowercase().to_string(),
                code: key_code(ch),
                vk: ch as u32,
                text: Some(ch.to_ascii_lowercase().to_string()),
                shifted: false,
                is_modifier: None,
            });
        }
    }
    if let Some(rest) = name.strip_prefix("Digit").filter(|s| s.len() == 1) {
        let ch = rest.chars().next().unwrap();
        if ch.is_ascii_digit() {
            return Ok(KeySpec {
                key: ch.to_string(),
                code: digit_code(ch),
                vk: ch as u32,
                text: Some(ch.to_string()),
                shifted: false,
                is_modifier: None,
            });
        }
    }
    let mut chars = name.chars();
    if let (Some(ch), None) = (chars.next(), chars.next()) {
        if ch.is_ascii_alphabetic() {
            let upper = ch.to_ascii_uppercase();
            return Ok(KeySpec {
                key: ch.to_string(),
                code: key_code(upper),
                vk: upper as u32,
                text: Some(ch.to_string()),
                shifted: ch.is_ascii_uppercase(),
                is_modifier: None,
            });
        }
        if ch.is_ascii_digit() {
            return Ok(KeySpec {
                key: ch.to_string(),
                code: digit_code(ch),
                vk: ch as u32,
                text: Some(ch.to_string()),
                shifted: false,
                is_modifier: None,
            });
        }
        if let Some((code, vk, shifted)) = punct_spec(ch) {
            return Ok(KeySpec {
                key: ch.to_string(),
                code,
                vk,
                text: Some(ch.to_string()),
                shifted,
                is_modifier: None,
            });
        }
        return Ok(KeySpec {
            key: ch.to_string(),
            code: "",
            vk: 0,
            text: Some(ch.to_string()),
            shifted: false,
            is_modifier: None,
        });
    }
    Err(Error::Other(format!("unknown key {name:?}")))
}

fn key_code(upper: char) -> &'static str {
    const CODES: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK",
        "KeyL", "KeyM", "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV",
        "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    CODES[(upper as usize) - ('A' as usize)]
}

fn digit_code(digit: char) -> &'static str {
    const CODES: [&str; 10] = [
        "Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8",
        "Digit9",
    ];
    CODES[(digit as usize) - ('0' as usize)]
}

fn punct_spec(ch: char) -> Option<(&'static str, u32, bool)> {
    Some(match ch {
        '-' => ("Minus", 189, false),
        '_' => ("Minus", 189, true),
        '=' => ("Equal", 187, false),
        '+' => ("Equal", 187, true),
        '[' => ("BracketLeft", 219, false),
        '{' => ("BracketLeft", 219, true),
        ']' => ("BracketRight", 221, false),
        '}' => ("BracketRight", 221, true),
        '\\' => ("Backslash", 220, false),
        '|' => ("Backslash", 220, true),
        ';' => ("Semicolon", 186, false),
        ':' => ("Semicolon", 186, true),
        '\'' => ("Quote", 222, false),
        '"' => ("Quote", 222, true),
        '`' => ("Backquote", 192, false),
        '~' => ("Backquote", 192, true),
        ',' => ("Comma", 188, false),
        '<' => ("Comma", 188, true),
        '.' => ("Period", 190, false),
        '>' => ("Period", 190, true),
        '/' => ("Slash", 191, false),
        '?' => ("Slash", 191, true),
        '!' => ("Digit1", 49, true),
        '@' => ("Digit2", 50, true),
        '#' => ("Digit3", 51, true),
        '$' => ("Digit4", 52, true),
        '%' => ("Digit5", 53, true),
        '^' => ("Digit6", 54, true),
        '&' => ("Digit7", 55, true),
        '*' => ("Digit8", 56, true),
        '(' => ("Digit9", 57, true),
        ')' => ("Digit0", 48, true),
        _ => return None,
    })
}

pub(crate) async fn dispatch_key_event(
    conn: &Connection,
    session: &str,
    down: bool,
    spec: &KeySpec,
    modifiers: u8,
    timeout: Duration,
) -> Result<()> {
    let shift = (modifiers & 8) != 0;
    let non_shift_mod = (modifiers & (1 | 2 | 4)) != 0;
    let effective_mods = modifiers | if !non_shift_mod && spec.shifted { 8 } else { 0 };
    let mut key = spec.key.clone();
    let mut text = spec.text.clone();
    if shift && !non_shift_mod && key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_lowercase() {
            let upper = ch.to_ascii_uppercase().to_string();
            key = upper.clone();
            text = Some(upper);
        }
    }
    let ty = if down {
        if text.is_some() && !non_shift_mod {
            "keyDown"
        } else {
            "rawKeyDown"
        }
    } else {
        "keyUp"
    };
    let mut params = json!({
        "type": ty,
        "key": key,
        "code": spec.code,
        "modifiers": effective_mods,
    });
    if spec.vk != 0 {
        params["windowsVirtualKeyCode"] = json!(spec.vk);
        params["nativeVirtualKeyCode"] = json!(spec.vk);
    }
    if down && !non_shift_mod {
        if let Some(t) = text {
            params["text"] = json!(t);
            params["unmodifiedText"] = json!(t);
        }
    }
    // Headless macOS Chromium needs explicit Cocoa editing commands for Cmd shortcuts.
    if cfg!(target_os = "macos") && down && (modifiers & 4) != 0 && (modifiers & (1 | 2)) == 0 {
        let cmd = match (key.to_ascii_lowercase().as_str(), shift) {
            ("a", false) => Some("selectAll"),
            ("c", false) => Some("copy"),
            ("x", false) => Some("cut"),
            ("v", false) => Some("paste"),
            ("z", false) => Some("undo"),
            ("z", true) => Some("redo"),
            _ => None,
        };
        if let Some(c) = cmd {
            params["commands"] = json!([c]);
        }
    }
    conn.call("Input.dispatchKeyEvent", params, Some(session), timeout)
        .await
        .map(|_| ())
}
