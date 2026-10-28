//! Target displays: computer use for agents (PLAN §19b, docs/api.md §17.10).
//!
//! The types here are the shared model of a display operation. The daemon
//! parses them from the API, sends them to the host agent in a `display`
//! frame (docs/host-protocol.md, "`display`"), and the host agent runs them
//! with a driver (`terrarium_host::display`). The CLI and the dashboard use
//! the same names.
//!
//! A key chord has one spelling for all backends: key names joined with
//! `-` or `+`, for example `ctrl-l`, `ctrl+alt+delete`, or `Return`.
//! [`KeyChord`] parses it once, and each backend renders it in its own
//! spelling: X keysyms for `xdotool` and `wtype` (and the RFB protocol of a
//! VNC server), and the vmlab chord names for `vmlab vm sendkeys`.

use serde::{Deserialize, Serialize};

/// The largest PNG that a screenshot may have, in bytes. The daemon caps
/// the output of a `display` session at this size, so the base64 answer
/// fits in the 16 MiB answer of the agent tunnel.
pub const MAX_SCREENSHOT_BYTES: usize = 8 * 1024 * 1024;
/// The most characters in one `type` operation.
pub const MAX_TYPE_CHARS: usize = 2000;
/// The largest template PNG of a `find` operation, in bytes.
pub const MAX_FIND_TEMPLATE_BYTES: usize = 512 * 1024;
/// Input operations on one target in one second, at most.
pub const INPUT_OPS_PER_SEC: u32 = 20;
/// The largest coordinate that a display operation takes.
pub const MAX_COORD: i32 = 32_767;
/// The largest scroll step count in one `scroll` operation.
pub const MAX_SCROLL: i32 = 100;

/// The operation names, in the order of docs/api.md §17.10.
pub const OP_NAMES: &[&str] = &[
    "info",
    "screenshot",
    "move",
    "click",
    "drag",
    "scroll",
    "keys",
    "type",
    "ocr",
    "find",
    "record",
];

/// A rectangle on the screen, in pixels from the top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    /// Parse `x,y,w,h` (commas or spaces).
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s
            .split([',', ' '])
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        let nums: Result<Vec<u32>, _> = parts.iter().map(|p| p.parse::<u32>()).collect();
        match nums.as_deref() {
            Ok([x, y, w, h]) => Self {
                x: *x,
                y: *y,
                w: *w,
                h: *h,
            }
            .checked(),
            _ => Err(format!(
                "bad region {s:?}: give x,y,w,h as four whole numbers, for example 0,0,800,600"
            )),
        }
    }

    /// Refuse an empty or a too large rectangle.
    pub fn checked(self) -> Result<Self, String> {
        let max = MAX_COORD as u32;
        if self.w == 0 || self.h == 0 {
            return Err("a region needs a width and a height above 0".into());
        }
        if self.x > max || self.y > max || self.w > max || self.h > max {
            return Err(format!("a region value is above {max}"));
        }
        Ok(self)
    }
}

/// A mouse button.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Button {
    #[default]
    Left,
    Right,
    Middle,
}

impl Button {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Middle => "middle",
        }
    }

    /// The X button number (`xdotool click N`, RFB button mask bit + 1).
    pub fn x_number(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Middle => 2,
            Self::Right => 3,
        }
    }
}

/// `record start` or `record stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordAction {
    Start,
    Stop,
}

/// One display operation. `#[serde(tag = "op")]`: the JSON is
/// `{"op": "click", "x": 10, "y": 20}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DisplayOp {
    /// The screen size, the backend, and what it can do.
    Info,
    /// A PNG of the screen, or of `region`, scaled by `scale` (0.1 to 1).
    Screenshot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<Region>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scale: Option<f32>,
    },
    /// Move the pointer to an absolute position.
    Move { x: i32, y: i32 },
    /// Click a button, at `x, y` when given, else where the pointer is.
    Click {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i32>,
        #[serde(default)]
        button: Button,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        double: bool,
    },
    /// Press the left button at `x1, y1`, move to `x2, y2`, and release.
    Drag { x1: i32, y1: i32, x2: i32, y2: i32 },
    /// Scroll by `dx` and `dy` steps (wheel clicks; a positive `dy`
    /// scrolls down), at `x, y` when given.
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i32>,
        #[serde(default)]
        dx: i32,
        #[serde(default)]
        dy: i32,
    },
    /// Press and release one key chord, for example `ctrl-l`.
    Keys { chord: String },
    /// Type unicode text.
    Type { text: String },
    /// Read the text on the screen, or in `region`.
    Ocr {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<Region>,
    },
    /// Find a template image (PNG, base64) on the screen.
    Find {
        image_base64: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        threshold: Option<f32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        region: Option<Region>,
    },
    /// Start or stop a recording of the screen (vmlab only).
    Record { action: RecordAction },
}

impl DisplayOp {
    /// The operation name, as in the API path.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Screenshot { .. } => "screenshot",
            Self::Move { .. } => "move",
            Self::Click { .. } => "click",
            Self::Drag { .. } => "drag",
            Self::Scroll { .. } => "scroll",
            Self::Keys { .. } => "keys",
            Self::Type { .. } => "type",
            Self::Ocr { .. } => "ocr",
            Self::Find { .. } => "find",
            Self::Record { .. } => "record",
        }
    }

    /// True for the operations that change the screen: they need an
    /// exclusive lease, a rate limit, and an audit row (docs/api.md §17.10).
    pub fn is_input(&self) -> bool {
        matches!(
            self,
            Self::Move { .. }
                | Self::Click { .. }
                | Self::Drag { .. }
                | Self::Scroll { .. }
                | Self::Keys { .. }
                | Self::Type { .. }
                | Self::Record { .. }
        )
    }

    /// Check the values. The error is a sentence for a `422`.
    pub fn check(&self) -> Result<(), String> {
        let coord = |what: &str, v: i32| {
            if (0..=MAX_COORD).contains(&v) {
                Ok(())
            } else {
                Err(format!("{what} {v} is outside 0..={MAX_COORD}"))
            }
        };
        let pair = |x: Option<i32>, y: Option<i32>| match (x, y) {
            (Some(x), Some(y)) => coord("x", x).and(coord("y", y)),
            (None, None) => Ok(()),
            _ => Err("give both x and y, or neither".to_owned()),
        };
        match self {
            Self::Info | Self::Record { .. } => Ok(()),
            Self::Screenshot { region, scale } => {
                if let Some(r) = region {
                    r.checked()?;
                }
                match scale {
                    Some(s) if !(0.1..=1.0).contains(s) || s.is_nan() => {
                        Err(format!("scale {s} is outside 0.1..=1"))
                    }
                    _ => Ok(()),
                }
            }
            Self::Move { x, y } => coord("x", *x).and(coord("y", *y)),
            Self::Click { x, y, .. } => pair(*x, *y),
            Self::Drag { x1, y1, x2, y2 } => coord("x1", *x1)
                .and(coord("y1", *y1))
                .and(coord("x2", *x2))
                .and(coord("y2", *y2)),
            Self::Scroll { x, y, dx, dy } => {
                pair(*x, *y)?;
                if *dx == 0 && *dy == 0 {
                    return Err("scroll needs a dx or a dy other than 0".into());
                }
                if dx.abs() > MAX_SCROLL || dy.abs() > MAX_SCROLL {
                    return Err(format!("scroll at most {MAX_SCROLL} steps at a time"));
                }
                Ok(())
            }
            Self::Keys { chord } => KeyChord::parse(chord).map(|_| ()),
            Self::Type { text } => {
                let n = text.chars().count();
                if n == 0 {
                    Err("text is empty".into())
                } else if n > MAX_TYPE_CHARS {
                    Err(format!(
                        "text has {n} characters; type at most {MAX_TYPE_CHARS} at a time"
                    ))
                } else if text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t')
                {
                    Err("text has control characters other than newline and tab; use `keys` for those".into())
                } else {
                    Ok(())
                }
            }
            Self::Ocr { region } => region.map_or(Ok(()), |r| r.checked().map(|_| ())),
            Self::Find {
                image_base64,
                threshold,
                region,
            } => {
                if image_base64.is_empty() {
                    return Err("image_base64 is empty".into());
                }
                // base64 is 4/3 of the bytes.
                if image_base64.len() > MAX_FIND_TEMPLATE_BYTES / 3 * 4 + 4 {
                    return Err(format!(
                        "the template image is over {} KiB",
                        MAX_FIND_TEMPLATE_BYTES / 1024
                    ));
                }
                if let Some(t) = threshold
                    && (!(0.0..=1.0).contains(t) || t.is_nan())
                {
                    return Err(format!("threshold {t} is outside 0..=1"));
                }
                region.map_or(Ok(()), |r| r.checked().map(|_| ()))
            }
        }
    }

    /// The audit detail of an input operation. `type` logs the length of
    /// the text, never the text.
    pub fn audit_detail(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            Self::Type { text } => json!({"op": "type", "chars": text.chars().count()}),
            Self::Find {
                threshold, region, ..
            } => {
                json!({"op": "find", "threshold": threshold, "region": region})
            }
            other => serde_json::to_value(other).unwrap_or_else(|_| json!({"op": other.name()})),
        }
    }
}

/// How a docker or desktop target shows its screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayProtocol {
    /// An X server (Xorg, Xvfb): `xdotool`, and `import`, `xwd`, or `scrot`.
    X11,
    /// A wlroots compositor (sway, cage, labwc): `grim`, `wtype`, `ydotool`.
    Wayland,
    /// The host agent looks: `xdotool` gives `x11`, else `grim` gives
    /// `wayland`.
    Auto,
}

impl DisplayProtocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::X11 => "x11",
            Self::Wayland => "wayland",
            Self::Auto => "auto",
        }
    }

    /// The default display name: `:0` for X11, `wayland-0` for Wayland.
    pub fn default_display(self) -> &'static str {
        match self {
            Self::Wayland => "wayland-0",
            Self::X11 | Self::Auto => ":0",
        }
    }
}

/// Check a display name: an X display (`:0`, `:1.0`, `host:0`) or a
/// Wayland socket (`wayland-0`, or an absolute socket path). It goes into
/// an environment variable and onto no command line.
pub fn check_display_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('-')
        && !name.split('/').any(|s| s == "..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-' | '/'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "bad display name {name:?}: an X display such as `:0`, or a Wayland socket such as `wayland-0`"
        ))
    }
}

// ------------------------------------------------------------------ keys

/// A modifier key of a chord.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modifier {
    Ctrl,
    Alt,
    Shift,
    Super,
}

impl Modifier {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ctrl" | "control" | "ctl" => Self::Ctrl,
            "alt" | "option" => Self::Alt,
            "shift" => Self::Shift,
            "super" | "win" | "meta" | "cmd" | "logo" => Self::Super,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ctrl => "ctrl",
            Self::Alt => "alt",
            Self::Shift => "shift",
            Self::Super => "super",
        }
    }

    /// The X keysym name (left key).
    pub fn x_keysym(self) -> &'static str {
        match self {
            Self::Ctrl => "Control_L",
            Self::Alt => "Alt_L",
            Self::Shift => "Shift_L",
            Self::Super => "Super_L",
        }
    }

    /// The X keysym value (left key), for the RFB protocol.
    pub fn x_keysym_value(self) -> u32 {
        match self {
            Self::Ctrl => 0xffe3,
            Self::Alt => 0xffe9,
            Self::Shift => 0xffe1,
            Self::Super => 0xffeb,
        }
    }

    /// The `xdotool key` spelling.
    fn xdotool(self) -> &'static str {
        self.as_str()
    }

    /// The `wtype -M` spelling.
    pub fn wtype(self) -> &'static str {
        match self {
            Self::Super => "logo",
            other => other.as_str(),
        }
    }

    /// The vmlab chord spelling.
    fn vmlab(self) -> &'static str {
        match self {
            Self::Super => "win",
            other => other.as_str(),
        }
    }
}

/// One named key: (canonical name, aliases, X keysym name, X keysym value,
/// vmlab name or "" when vmlab has none).
type NamedKey = (
    &'static str,
    &'static [&'static str],
    &'static str,
    u32,
    &'static str,
);

/// The named keys.
const NAMED: &[NamedKey] = &[
    ("enter", &["return", "ret"], "Return", 0xff0d, "enter"),
    ("tab", &[], "Tab", 0xff09, "tab"),
    ("backspace", &["bksp"], "BackSpace", 0xff08, "backspace"),
    ("space", &["spc"], "space", 0x20, "space"),
    ("escape", &["esc"], "Escape", 0xff1b, "esc"),
    ("delete", &["del"], "Delete", 0xffff, "delete"),
    ("insert", &["ins"], "Insert", 0xff63, "insert"),
    ("up", &[], "Up", 0xff52, "up"),
    ("down", &[], "Down", 0xff54, "down"),
    ("left", &[], "Left", 0xff51, "left"),
    ("right", &[], "Right", 0xff53, "right"),
    ("home", &[], "Home", 0xff50, "home"),
    ("end", &[], "End", 0xff57, "end"),
    ("pageup", &["pgup", "prior"], "Prior", 0xff55, "pgup"),
    ("pagedown", &["pgdn", "next"], "Next", 0xff56, "pgdn"),
    ("menu", &[], "Menu", 0xff67, "menu"),
    ("print", &["printscreen", "prtsc"], "Print", 0xff61, "print"),
    ("pause", &[], "Pause", 0xff13, "pause"),
    ("capslock", &["caps_lock"], "Caps_Lock", 0xffe5, "caps_lock"),
    ("numlock", &["num_lock"], "Num_Lock", 0xff7f, "num_lock"),
    (
        "scrolllock",
        &["scroll_lock"],
        "Scroll_Lock",
        0xff14,
        "scroll_lock",
    ),
    // Punctuation, by name (US layout). vmlab chords have no name for them.
    ("minus", &["-"], "minus", 0x2d, ""),
    ("equal", &["="], "equal", 0x3d, ""),
    ("bracketleft", &["["], "bracketleft", 0x5b, ""),
    ("bracketright", &["]"], "bracketright", 0x5d, ""),
    ("backslash", &["\\"], "backslash", 0x5c, ""),
    ("semicolon", &[";"], "semicolon", 0x3b, ""),
    ("apostrophe", &["'"], "apostrophe", 0x27, ""),
    ("comma", &[","], "comma", 0x2c, ""),
    ("period", &["dot", "."], "period", 0x2e, ""),
    ("slash", &["/"], "slash", 0x2f, ""),
    ("grave", &["`"], "grave", 0x60, ""),
];

/// The key of a chord: a named key, a letter or a digit, or a function key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "name")]
pub enum Key {
    /// A canonical name of [`NAMED`], for example `enter`.
    Named(String),
    /// A lowercase ASCII letter or a digit.
    Char(char),
    /// F1 to F24.
    F(u8),
}

impl Key {
    fn parse(s: &str) -> Option<Self> {
        let lower = s.to_ascii_lowercase();
        if let Some((canon, ..)) = NAMED
            .iter()
            .find(|(c, aliases, ..)| *c == lower || aliases.contains(&lower.as_str()))
        {
            return Some(Self::Named((*canon).to_owned()));
        }
        let mut chars = lower.chars();
        if let (Some(c), None) = (chars.next(), chars.next())
            && c.is_ascii_alphanumeric()
        {
            return Some(Self::Char(c));
        }
        if let Some(n) = lower.strip_prefix('f')
            && let Ok(n) = n.parse::<u8>()
            && (1..=24).contains(&n)
        {
            return Some(Self::F(n));
        }
        None
    }

    fn named(&self) -> Option<&'static NamedKey> {
        match self {
            Self::Named(n) => NAMED.iter().find(|e| e.0 == n),
            _ => None,
        }
    }

    /// The canonical name.
    pub fn name(&self) -> String {
        match self {
            Self::Named(n) => n.clone(),
            Self::Char(c) => c.to_string(),
            Self::F(n) => format!("f{n}"),
        }
    }

    /// The X keysym name, for `xdotool` and `wtype -k`.
    pub fn x_keysym(&self) -> String {
        match self {
            Self::Named(_) => self.named().map_or("", |e| e.2).to_owned(),
            Self::Char(c) => c.to_string(),
            Self::F(n) => format!("F{n}"),
        }
    }

    /// The X keysym value, for the RFB protocol of a VNC server.
    pub fn x_keysym_value(&self) -> u32 {
        match self {
            Self::Named(_) => self.named().map_or(0, |e| e.3),
            Self::Char(c) => *c as u32,
            Self::F(n) => 0xffbe + u32::from(*n) - 1,
        }
    }

    /// The vmlab chord name, or `None` when vmlab has no name for the key
    /// (punctuation, and F13 to F24).
    pub fn vmlab(&self) -> Option<String> {
        match self {
            Self::Named(_) => self
                .named()
                .map(|e| e.4)
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
            Self::Char(c) => Some(c.to_string()),
            Self::F(n) if *n <= 12 => Some(format!("f{n}")),
            Self::F(_) => None,
        }
    }
}

/// A parsed key chord: modifiers held while one key is pressed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyChord {
    pub modifiers: Vec<Modifier>,
    pub key: Key,
}

impl KeyChord {
    /// Parse `ctrl-l`, `ctrl+alt+delete`, `Return`, `shift-f10`, or `-`.
    /// The separator is `+` when the chord holds one, else `-`. The names
    /// do not depend on case.
    pub fn parse(chord: &str) -> Result<Self, String> {
        let chord = chord.trim();
        if chord.is_empty() {
            return Err("the key chord is empty".into());
        }
        if chord.len() > 64 || chord.chars().any(|c| c.is_control() || c == ' ') {
            return Err(format!("bad key chord {chord:?}"));
        }
        let parts: Vec<String> = if chord.len() == 1 {
            vec![chord.to_owned()]
        } else if chord.contains('+') && chord != "+" {
            split_keep_trailing(chord, '+')
        } else {
            split_keep_trailing(chord, '-')
        };
        let (last, mods) = parts
            .split_last()
            .ok_or_else(|| "the key chord is empty".to_owned())?;
        let mut modifiers = Vec::new();
        for m in mods {
            let m = Modifier::parse(&m.to_ascii_lowercase()).ok_or_else(|| {
                format!("unknown modifier `{m}` in chord {chord:?}: use ctrl, alt, shift, or super")
            })?;
            if !modifiers.contains(&m) {
                modifiers.push(m);
            }
        }
        let lower = last.to_ascii_lowercase();
        let key = match Key::parse(last) {
            Some(k) => k,
            // A chord of modifiers only, such as `shift`.
            None if mods.is_empty() && Modifier::parse(&lower).is_some() => {
                return Err(format!(
                    "chord {chord:?} has no key: press a modifier with a key, for example ctrl-l"
                ));
            }
            None => {
                return Err(format!(
                    "unknown key `{last}` in chord {chord:?}: use a letter, a digit, f1 to f24, \
                     or a name such as enter, tab, escape, space, backspace, delete, up, pageup"
                ));
            }
        };
        Ok(Self { modifiers, key })
    }

    /// The `xdotool key` argument: `ctrl+alt+Delete`.
    pub fn xdotool(&self) -> String {
        let mut parts: Vec<String> = self
            .modifiers
            .iter()
            .map(|m| m.xdotool().to_owned())
            .collect();
        parts.push(self.key.x_keysym());
        parts.join("+")
    }

    /// The `wtype` arguments: `-M ctrl -k l -m ctrl`.
    pub fn wtype_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        for m in &self.modifiers {
            a.push("-M".to_owned());
            a.push(m.wtype().to_owned());
        }
        a.push("-k".to_owned());
        a.push(self.key.x_keysym());
        for m in self.modifiers.iter().rev() {
            a.push("-m".to_owned());
            a.push(m.wtype().to_owned());
        }
        a
    }

    /// The `vmlab vm sendkeys` chord: `ctrl-alt-delete`. An error for a
    /// key that vmlab has no name for.
    pub fn vmlab(&self) -> Result<String, String> {
        let key = self.key.vmlab().ok_or_else(|| {
            format!(
                "vmlab has no chord name for the key `{}`: vmlab chords take letters, digits, \
                 f1 to f12, and named keys such as enter, tab, esc, space, delete, up, pgup",
                self.key.name()
            )
        })?;
        let mut parts: Vec<String> = self
            .modifiers
            .iter()
            .map(|m| m.vmlab().to_owned())
            .collect();
        parts.push(key);
        Ok(parts.join("-"))
    }

    /// The canonical spelling: `ctrl-alt-delete`.
    pub fn canonical(&self) -> String {
        let mut parts: Vec<String> = self
            .modifiers
            .iter()
            .map(|m| m.as_str().to_owned())
            .collect();
        parts.push(self.key.name());
        parts.join("-")
    }
}

/// Split on `sep`, so that a trailing doubled separator (`ctrl--`,
/// `ctrl++`) keeps the separator as the last key.
fn split_keep_trailing(s: &str, sep: char) -> Vec<String> {
    let doubled = format!("{sep}{sep}");
    if let Some(head) = s.strip_suffix(&doubled) {
        let mut parts: Vec<String> = head.split(sep).map(str::to_owned).collect();
        parts.push(sep.to_string());
        return parts;
    }
    s.split(sep).map(str::to_owned).collect()
}

// ------------------------------------------------------- terminal keys

/// The most keys in one `POST /api/agents/{id}/keys` request.
pub const MAX_TERMINAL_KEYS: usize = 20;
/// Terminal keys to one agent in one second, at most.
pub const TERMINAL_KEYS_PER_SEC: u32 = 20;
/// The most characters of one `text:` key.
pub const MAX_TERMINAL_TEXT: usize = 2000;
/// The prefix of a literal text key: `text:abc` types `abc`.
pub const TEXT_KEY_PREFIX: &str = "text:";

/// One key press for the harness TUI of an agent (`POST
/// /api/agents/{id}/keys`, `terrarium agent keys`): a [`KeyChord`] in the
/// spelling of the display keys, or `text:<text>` for literal text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalKey {
    Chord(KeyChord),
    Text(String),
}

impl TerminalKey {
    /// Parse `esc`, `down`, `ctrl-c`, `shift-tab`, `f5`, or `text:abc`. A
    /// chord that a terminal cannot send (for example `super-e`, `print`)
    /// is an error.
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(text) = s.strip_prefix(TEXT_KEY_PREFIX) {
            if text.is_empty() {
                return Err("`text:` needs the text after the colon".into());
            }
            if text.chars().count() > MAX_TERMINAL_TEXT {
                return Err(format!(
                    "a `text:` key has at most {MAX_TERMINAL_TEXT} characters"
                ));
            }
            return Ok(Self::Text(text.to_owned()));
        }
        let chord = KeyChord::parse(s)?;
        chord.terminal_bytes()?;
        Ok(Self::Chord(chord))
    }

    /// The bytes to write to the PTY. A text key is written as-is, without
    /// the bracketed-paste markers.
    pub fn bytes(&self) -> Vec<u8> {
        match self {
            Self::Chord(c) => c.terminal_bytes().unwrap_or_default(),
            Self::Text(t) => t
                .replace("\x1b[200~", "")
                .replace("\x1b[201~", "")
                .into_bytes(),
        }
    }

    /// The name for the audit log: the canonical chord, or `text` with
    /// its length only (never the text).
    pub fn audit_name(&self) -> String {
        match self {
            Self::Chord(c) => c.canonical(),
            Self::Text(t) => format!("text({})", t.chars().count()),
        }
    }
}

/// The xterm modifier parameter of a CSI key: 1 + shift (1) + alt (2) +
/// ctrl (4).
fn xterm_modifier(mods: &[Modifier]) -> Option<u8> {
    let mut m = 1;
    for x in mods {
        m += match x {
            Modifier::Shift => 1,
            Modifier::Alt => 2,
            Modifier::Ctrl => 4,
            Modifier::Super => return None,
        };
    }
    Some(m)
}

impl KeyChord {
    /// The bytes that an xterm-compatible terminal sends for the chord
    /// (the `TERM=xterm-256color` of the agent PTY):
    ///
    /// | Key | Bytes |
    /// |---|---|
    /// | `enter`, `tab`, `shift-tab`, `backspace`, `escape`, `space` | CR, HT, `ESC [ Z`, DEL, ESC, space |
    /// | `up`, `down`, `right`, `left` | `ESC [ A` .. `ESC [ D`; with modifiers `ESC [ 1 ; <m> A` |
    /// | `home`, `end` | `ESC [ H`, `ESC [ F` (with modifiers as the arrows) |
    /// | `insert`, `delete`, `pageup`, `pagedown` | `ESC [ 2 ~`, `3 ~`, `5 ~`, `6 ~`; with modifiers `ESC [ <n> ; <m> ~` |
    /// | `f1` .. `f4` | `ESC O P` .. `ESC O S` |
    /// | `f5` .. `f12` | `ESC [ 15 ~`, `17 ~`, `18 ~`, `19 ~`, `20 ~`, `21 ~`, `23 ~`, `24 ~` |
    /// | `ctrl-<letter>` | the control code (`ctrl-c` is 0x03) |
    /// | `ctrl-space`, `ctrl-[`, `ctrl-\`, `ctrl-]` | NUL, ESC, 0x1c, 0x1d |
    /// | `alt-<key>` | ESC, then the key |
    /// | a letter, a digit, punctuation | the character (`shift-a` is `A`) |
    ///
    /// `<m>` is 1 + shift (1) + alt (2) + ctrl (4). An error for `super`,
    /// F13 to F24, and keys a terminal has no code for (`print`, `menu`,
    /// `capslock`).
    pub fn terminal_bytes(&self) -> Result<Vec<u8>, String> {
        let mods = &self.modifiers;
        let has = |m: Modifier| mods.contains(&m);
        let no_terminal = || {
            format!(
                "a terminal has no key code for {:?}: use enter, tab, escape, backspace, \
                 space, the arrows, home, end, pageup, pagedown, insert, delete, f1 to f12, \
                 a letter, a digit, or ctrl-, alt-, shift- with them",
                self.canonical()
            )
        };
        if has(Modifier::Super) {
            return Err(no_terminal());
        }
        let m = xterm_modifier(mods).ok_or_else(no_terminal)?;
        let csi_letter = |c: char| -> Vec<u8> {
            if m == 1 {
                format!("\x1b[{c}").into_bytes()
            } else {
                format!("\x1b[1;{m}{c}").into_bytes()
            }
        };
        let csi_tilde = |n: u8| -> Vec<u8> {
            if m == 1 {
                format!("\x1b[{n}~").into_bytes()
            } else {
                format!("\x1b[{n};{m}~").into_bytes()
            }
        };
        let alt_prefix = |mut b: Vec<u8>| -> Vec<u8> {
            if has(Modifier::Alt) {
                b.insert(0, 0x1b);
            }
            b
        };
        match &self.key {
            Key::F(n) => {
                let ss3 = |c: char| -> Vec<u8> {
                    if m == 1 {
                        format!("\x1bO{c}").into_bytes()
                    } else {
                        format!("\x1b[1;{m}{c}").into_bytes()
                    }
                };
                match n {
                    1 => Ok(ss3('P')),
                    2 => Ok(ss3('Q')),
                    3 => Ok(ss3('R')),
                    4 => Ok(ss3('S')),
                    5 => Ok(csi_tilde(15)),
                    6 => Ok(csi_tilde(17)),
                    7 => Ok(csi_tilde(18)),
                    8 => Ok(csi_tilde(19)),
                    9 => Ok(csi_tilde(20)),
                    10 => Ok(csi_tilde(21)),
                    11 => Ok(csi_tilde(23)),
                    12 => Ok(csi_tilde(24)),
                    _ => Err(no_terminal()),
                }
            }
            Key::Char(c) => {
                let c = *c;
                if has(Modifier::Ctrl) {
                    let code = if c.is_ascii_lowercase() {
                        (c as u8) & 0x1f
                    } else {
                        match c {
                            '2' => 0x00,
                            '3' => 0x1b,
                            '4' => 0x1c,
                            '5' => 0x1d,
                            '6' => 0x1e,
                            '7' => 0x1f,
                            '8' => 0x7f,
                            _ => return Err(no_terminal()),
                        }
                    };
                    return Ok(alt_prefix(vec![code]));
                }
                let ch = if has(Modifier::Shift) {
                    c.to_ascii_uppercase()
                } else {
                    c
                };
                Ok(alt_prefix(vec![ch as u8]))
            }
            Key::Named(name) => {
                let ch_key = |plain: u8| -> Result<Vec<u8>, String> {
                    if has(Modifier::Ctrl) {
                        let code = match plain {
                            b' ' => 0x00,
                            b'[' => 0x1b,
                            b'\\' => 0x1c,
                            b']' => 0x1d,
                            b'/' | b'-' => 0x1f,
                            _ => return Err(no_terminal()),
                        };
                        return Ok(alt_prefix(vec![code]));
                    }
                    Ok(alt_prefix(vec![plain]))
                };
                match name.as_str() {
                    "enter" => Ok(alt_prefix(vec![b'\r'])),
                    "tab" if has(Modifier::Shift) => Ok(b"\x1b[Z".to_vec()),
                    "tab" => Ok(alt_prefix(vec![b'\t'])),
                    "backspace" if has(Modifier::Ctrl) => Ok(alt_prefix(vec![0x08])),
                    "backspace" => Ok(alt_prefix(vec![0x7f])),
                    "escape" => Ok(alt_prefix(vec![0x1b])),
                    "space" => ch_key(b' '),
                    "up" => Ok(csi_letter('A')),
                    "down" => Ok(csi_letter('B')),
                    "right" => Ok(csi_letter('C')),
                    "left" => Ok(csi_letter('D')),
                    "home" => Ok(csi_letter('H')),
                    "end" => Ok(csi_letter('F')),
                    "insert" => Ok(csi_tilde(2)),
                    "delete" => Ok(csi_tilde(3)),
                    "pageup" => Ok(csi_tilde(5)),
                    "pagedown" => Ok(csi_tilde(6)),
                    _ => {
                        let entry = self.key.named().ok_or_else(no_terminal)?;
                        // Punctuation: the X keysym value is the character.
                        let v = entry.3;
                        if (0x21..0x7f).contains(&v) {
                            let b = u8::try_from(v).map_err(|_| no_terminal())?;
                            let b = if has(Modifier::Shift) {
                                us_shifted(b).ok_or_else(no_terminal)?
                            } else {
                                b
                            };
                            ch_key(b)
                        } else {
                            Err(no_terminal())
                        }
                    }
                }
            }
        }
    }
}

/// The character that Shift gives on a US keyboard for punctuation `b`.
fn us_shifted(b: u8) -> Option<u8> {
    Some(match b {
        b'-' => b'_',
        b'=' => b'+',
        b'[' => b'{',
        b']' => b'}',
        b'\\' => b'|',
        b';' => b':',
        b'\'' => b'"',
        b',' => b'<',
        b'.' => b'>',
        b'/' => b'?',
        b'`' => b'~',
        _ => return None,
    })
}

/// The chord that types one character on a US keyboard, for a backend
/// that has no text input. `None` for a character that the US layout has
/// no key for.
pub fn us_char_chord(c: char) -> Option<KeyChord> {
    const SHIFTED: &[(char, &str)] = &[
        ('!', "1"),
        ('@', "2"),
        ('#', "3"),
        ('$', "4"),
        ('%', "5"),
        ('^', "6"),
        ('&', "7"),
        ('*', "8"),
        ('(', "9"),
        (')', "0"),
        ('_', "minus"),
        ('+', "equal"),
        ('{', "bracketleft"),
        ('}', "bracketright"),
        ('|', "backslash"),
        (':', "semicolon"),
        ('"', "apostrophe"),
        ('<', "comma"),
        ('>', "period"),
        ('?', "slash"),
        ('~', "grave"),
    ];
    let plain = |k: &str| {
        Some(KeyChord {
            modifiers: vec![],
            key: Key::parse(k)?,
        })
    };
    let shifted = |k: &str| {
        Some(KeyChord {
            modifiers: vec![Modifier::Shift],
            key: Key::parse(k)?,
        })
    };
    match c {
        'a'..='z' | '0'..='9' => plain(&c.to_string()),
        'A'..='Z' => shifted(&c.to_ascii_lowercase().to_string()),
        ' ' => plain("space"),
        '\n' => plain("enter"),
        '\t' => plain("tab"),
        '-' | '=' | '[' | ']' | '\\' | ';' | '\'' | ',' | '.' | '/' | '`' => plain(&c.to_string()),
        _ => SHIFTED
            .iter()
            .find(|(s, _)| *s == c)
            .and_then(|(_, k)| shifted(k)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chords_parse_in_both_spellings() {
        let c = KeyChord::parse("ctrl-l").unwrap();
        assert_eq!(c.modifiers, vec![Modifier::Ctrl]);
        assert_eq!(c.key, Key::Char('l'));
        assert_eq!(c.xdotool(), "ctrl+l");
        assert_eq!(c.vmlab().unwrap(), "ctrl-l");
        assert_eq!(c.wtype_args(), ["-M", "ctrl", "-k", "l", "-m", "ctrl"]);

        let c = KeyChord::parse("Ctrl+Alt+Delete").unwrap();
        assert_eq!(c.xdotool(), "ctrl+alt+Delete");
        assert_eq!(c.vmlab().unwrap(), "ctrl-alt-delete");
        assert_eq!(c.canonical(), "ctrl-alt-delete");

        assert_eq!(KeyChord::parse("Return").unwrap().xdotool(), "Return");
        assert_eq!(KeyChord::parse("enter").unwrap().vmlab().unwrap(), "enter");
        assert_eq!(
            KeyChord::parse("super-e").unwrap().vmlab().unwrap(),
            "win-e"
        );
        assert_eq!(KeyChord::parse("shift-f10").unwrap().xdotool(), "shift+F10");
        assert_eq!(KeyChord::parse("PgDn").unwrap().xdotool(), "Next");
        assert_eq!(KeyChord::parse("A").unwrap().key, Key::Char('a'));
    }

    #[test]
    fn the_separator_can_be_a_key() {
        assert_eq!(KeyChord::parse("-").unwrap().key.name(), "minus");
        assert_eq!(KeyChord::parse("ctrl--").unwrap().xdotool(), "ctrl+minus");
        assert_eq!(KeyChord::parse("ctrl+-").unwrap().xdotool(), "ctrl+minus");
        assert_eq!(
            KeyChord::parse("ctrl+minus").unwrap().xdotool(),
            "ctrl+minus"
        );
        assert!(KeyChord::parse("ctrl++").is_err(), "+ has no key name");
    }

    /// The key map of `POST /api/agents/{id}/keys` (docs/api.md §6.5).
    #[test]
    fn terminal_key_map() {
        let b = |s: &str| TerminalKey::parse(s).unwrap().bytes();
        for (key, want) in [
            ("esc", &b"\x1b"[..]),
            ("escape", b"\x1b"),
            ("enter", b"\r"),
            ("Return", b"\r"),
            ("tab", b"\t"),
            ("shift-tab", b"\x1b[Z"),
            ("backspace", b"\x7f"),
            ("space", b" "),
            ("up", b"\x1b[A"),
            ("down", b"\x1b[B"),
            ("right", b"\x1b[C"),
            ("left", b"\x1b[D"),
            ("ctrl-up", b"\x1b[1;5A"),
            ("shift-right", b"\x1b[1;2C"),
            ("home", b"\x1b[H"),
            ("end", b"\x1b[F"),
            ("pageup", b"\x1b[5~"),
            ("pgdn", b"\x1b[6~"),
            ("delete", b"\x1b[3~"),
            ("insert", b"\x1b[2~"),
            ("ctrl-delete", b"\x1b[3;5~"),
            ("f1", b"\x1bOP"),
            ("f4", b"\x1bOS"),
            ("f5", b"\x1b[15~"),
            ("f12", b"\x1b[24~"),
            ("shift-f1", b"\x1b[1;2P"),
            ("ctrl-c", b"\x03"),
            ("ctrl-d", b"\x04"),
            ("ctrl+l", b"\x0c"),
            ("ctrl-space", b"\x00"),
            ("ctrl-[", b"\x1b"),
            ("ctrl-]", b"\x1d"),
            ("alt-b", b"\x1bb"),
            ("alt-enter", b"\x1b\r"),
            ("a", b"a"),
            ("shift-a", b"A"),
            ("7", b"7"),
            ("/", b"/"),
            ("shift-/", b"?"),
            ("text:/compact", b"/compact"),
            ("text:a\x1b[201~b", b"ab"),
        ] {
            assert_eq!(b(key), want.to_vec(), "{key}");
        }
        for bad in [
            "super-e", "f13", "print", "menu", "capslock", "text:", "hyper-x", "",
        ] {
            assert!(TerminalKey::parse(bad).is_err(), "{bad}");
        }
        let long = format!("text:{}", "x".repeat(MAX_TERMINAL_TEXT + 1));
        assert!(TerminalKey::parse(&long).is_err());
        assert_eq!(TerminalKey::parse("Esc").unwrap().audit_name(), "escape");
        assert_eq!(TerminalKey::parse("ctrl+c").unwrap().audit_name(), "ctrl-c");
        assert_eq!(
            TerminalKey::parse("text:secret").unwrap().audit_name(),
            "text(6)"
        );
    }

    #[test]
    fn bad_chords_are_refused() {
        assert!(KeyChord::parse("").is_err());
        assert!(KeyChord::parse("ctrl").is_err());
        assert!(KeyChord::parse("hyper-x").is_err());
        assert!(KeyChord::parse("ctrl-flurb").is_err());
        assert!(KeyChord::parse("ctrl l").is_err());
        assert!(KeyChord::parse("f25").is_err());
    }

    #[test]
    fn vmlab_has_no_punctuation_names() {
        let e = KeyChord::parse("ctrl-period").unwrap().vmlab().unwrap_err();
        assert!(e.contains("vmlab has no chord name"), "{e}");
        assert!(KeyChord::parse("f13").unwrap().vmlab().is_err());
        assert_eq!(KeyChord::parse("f12").unwrap().vmlab().unwrap(), "f12");
    }

    #[test]
    fn keysym_values_for_rfb() {
        assert_eq!(KeyChord::parse("a").unwrap().key.x_keysym_value(), 0x61);
        assert_eq!(
            KeyChord::parse("enter").unwrap().key.x_keysym_value(),
            0xff0d
        );
        assert_eq!(KeyChord::parse("f1").unwrap().key.x_keysym_value(), 0xffbe);
        assert_eq!(KeyChord::parse("f12").unwrap().key.x_keysym_value(), 0xffc9);
        assert_eq!(Modifier::Ctrl.x_keysym_value(), 0xffe3);
    }

    #[test]
    fn us_chords_for_text() {
        assert_eq!(us_char_chord('a').unwrap().xdotool(), "a");
        assert_eq!(us_char_chord('A').unwrap().xdotool(), "shift+a");
        assert_eq!(us_char_chord('!').unwrap().vmlab().unwrap(), "shift-1");
        assert_eq!(us_char_chord('\n').unwrap().vmlab().unwrap(), "enter");
        assert_eq!(us_char_chord('.').unwrap().xdotool(), "period");
        assert!(us_char_chord('.').unwrap().vmlab().is_err());
        assert!(us_char_chord('é').is_none());
    }

    #[test]
    fn regions_parse_and_check() {
        assert_eq!(
            Region::parse("10,20,300,400").unwrap(),
            Region {
                x: 10,
                y: 20,
                w: 300,
                h: 400
            }
        );
        assert!(Region::parse("1,2,3").is_err());
        assert!(Region::parse("0,0,0,10").is_err());
        assert!(Region::parse("a,b,c,d").is_err());
    }

    #[test]
    fn ops_round_trip_and_check() {
        let op: DisplayOp =
            serde_json::from_value(json!({"op": "click", "x": 10, "y": 20, "button": "right"}))
                .unwrap();
        assert_eq!(
            op,
            DisplayOp::Click {
                x: Some(10),
                y: Some(20),
                button: Button::Right,
                double: false
            }
        );
        assert!(op.is_input());
        assert!(op.check().is_ok());
        assert!(!DisplayOp::Info.is_input());
        let bad = DisplayOp::Click {
            x: Some(1),
            y: None,
            button: Button::Left,
            double: false,
        };
        assert!(bad.check().is_err());
        assert!(DisplayOp::Move { x: -1, y: 0 }.check().is_err());
        assert!(
            DisplayOp::Screenshot {
                region: None,
                scale: Some(2.0)
            }
            .check()
            .is_err()
        );
        assert!(
            DisplayOp::Scroll {
                x: None,
                y: None,
                dx: 0,
                dy: 0
            }
            .check()
            .is_err()
        );
        let t = DisplayOp::Type {
            text: "secret".into(),
        };
        assert_eq!(t.audit_detail(), json!({"op": "type", "chars": 6}));
        assert!(!t.audit_detail().to_string().contains("secret"));
        assert!(
            DisplayOp::Type {
                text: "a\u{7}b".into()
            }
            .check()
            .is_err()
        );
        for name in OP_NAMES {
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn display_names() {
        assert!(check_display_name(":0").is_ok());
        assert!(check_display_name(":1.0").is_ok());
        assert!(check_display_name("wayland-1").is_ok());
        assert!(check_display_name("/tmp/xdg/wayland-0").is_ok());
        assert!(check_display_name("-x").is_err());
        assert!(check_display_name(":0 ; rm").is_err());
        assert!(check_display_name("/tmp/../etc").is_err());
    }
}
