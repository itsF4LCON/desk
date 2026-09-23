//! Remote input → Linux uinput. One absolute pointer (spanning the whole monitor layout) and
//! one keyboard. Every pressed key/button is tracked so it can be released on disconnect.

use std::collections::HashSet;

use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, PropType,
    RelativeAxisCode, UinputAbsSetup,
};
use serde::Deserialize;

/// Geometry of the captured monitor inside the compositor layout (logical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// Messages from the browser's `input` data channel.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "t")]
pub enum InputMsg {
    /// Pointer position, normalised 0..1 over the video content.
    #[serde(rename = "m")]
    Move { x: f64, y: f64 },
    /// Mouse button (0 left, 1 middle, 2 right — DOM `MouseEvent.button`).
    #[serde(rename = "b")]
    Button { b: u8, d: bool },
    /// Wheel in DOM "lines"; positive dy scrolls down.
    #[serde(rename = "w")]
    Wheel { dx: f64, dy: f64 },
    /// Key by DOM `KeyboardEvent.code`.
    #[serde(rename = "k")]
    Key { c: String, d: bool },
    /// Release everything (tab blurred, pointer left, etc.).
    #[serde(rename = "rel")]
    ReleaseAll,
    /// Heartbeat; lets the agent release keys if the viewer silently disappears.
    #[serde(rename = "hb")]
    Heartbeat,
}

pub struct Injector {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    layout: Rect,
    monitor: Rect,
    held_keys: HashSet<KeyCode>,
    wheel_acc: (f64, f64),
}

const POINTER_BUTTONS: [KeyCode; 3] = [KeyCode::BTN_LEFT, KeyCode::BTN_MIDDLE, KeyCode::BTN_RIGHT];

impl Injector {
    /// `layout` is the bounding box of all monitors; the absolute axes span exactly that box.
    pub fn new(layout: Rect, monitor: Rect) -> Result<Self> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in POINTER_BUTTONS {
            buttons.insert(b);
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        rel.insert(RelativeAxisCode::REL_WHEEL);
        rel.insert(RelativeAxisCode::REL_HWHEEL);
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::POINTER);
        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, layout.w - 1, 0, 0, 0),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, layout.h - 1, 0, 0, 0),
        );
        let pointer = VirtualDevice::builder()
            .context("cannot open /dev/uinput")?
            .name("desk-agent pointer")
            .with_keys(&buttons)?
            .with_relative_axes(&rel)?
            .with_absolute_axis(&abs_x)?
            .with_absolute_axis(&abs_y)?
            .with_properties(&props)?
            .build()?;

        let mut keys = AttributeSet::<KeyCode>::new();
        for (_, k) in KEYMAP {
            keys.insert(*k);
        }
        let keyboard = VirtualDevice::builder()?
            .name("desk-agent keyboard")
            .with_keys(&keys)?
            .build()?;

        Ok(Self {
            pointer,
            keyboard,
            layout,
            monitor,
            held_keys: HashSet::new(),
            wheel_acc: (0.0, 0.0),
        })
    }

    pub fn set_monitor(&mut self, monitor: Rect) {
        self.release_all();
        self.monitor = monitor;
    }

    pub fn handle(&mut self, msg: InputMsg) -> Result<()> {
        match msg {
            InputMsg::Move { x, y } => {
                let (ax, ay) = map_point(self.layout, self.monitor, x, y);
                self.pointer.emit(&[
                    InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, ax),
                    InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, ay),
                ])?;
            }
            InputMsg::Button { b, d } => {
                let code = match b {
                    0 => KeyCode::BTN_LEFT,
                    1 => KeyCode::BTN_MIDDLE,
                    2 => KeyCode::BTN_RIGHT,
                    _ => return Ok(()),
                };
                self.pointer
                    .emit(&[InputEvent::new(EventType::KEY.0, code.0, d as i32)])?;
                self.track(code, d);
            }
            InputMsg::Wheel { dx, dy } => {
                // Accumulate fractional lines (trackpads) and emit whole detents.
                self.wheel_acc.0 += dx.clamp(-20.0, 20.0);
                self.wheel_acc.1 += dy.clamp(-20.0, 20.0);
                let (sx, sy) = (self.wheel_acc.0.trunc(), self.wheel_acc.1.trunc());
                self.wheel_acc.0 -= sx;
                self.wheel_acc.1 -= sy;
                let mut ev = Vec::new();
                if sy != 0.0 {
                    ev.push(InputEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_WHEEL.0,
                        -sy as i32,
                    ));
                }
                if sx != 0.0 {
                    ev.push(InputEvent::new(
                        EventType::RELATIVE.0,
                        RelativeAxisCode::REL_HWHEEL.0,
                        sx as i32,
                    ));
                }
                if !ev.is_empty() {
                    self.pointer.emit(&ev)?;
                }
            }
            InputMsg::Key { c, d } => {
                let Some(code) = keycode(&c) else {
                    return Ok(());
                };
                // Ignore key-repeat "down" events for keys we already hold; the PC repeats itself.
                if d && self.held_keys.contains(&code) {
                    return Ok(());
                }
                self.keyboard
                    .emit(&[InputEvent::new(EventType::KEY.0, code.0, d as i32)])?;
                self.track(code, d);
            }
            InputMsg::ReleaseAll => self.release_all(),
            InputMsg::Heartbeat => {}
        }
        Ok(())
    }

    fn track(&mut self, code: KeyCode, down: bool) {
        if down {
            self.held_keys.insert(code);
        } else {
            self.held_keys.remove(&code);
        }
    }

    /// Releases every held key and button. Safe to call at any time.
    pub fn release_all(&mut self) {
        for code in std::mem::take(&mut self.held_keys) {
            let dev = if POINTER_BUTTONS.contains(&code) {
                &mut self.pointer
            } else {
                &mut self.keyboard
            };
            let _ = dev.emit(&[InputEvent::new(EventType::KEY.0, code.0, 0)]);
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release_all();
    }
}

/// Normalised video coordinates → absolute axis values spanning the layout box.
pub fn map_point(layout: Rect, monitor: Rect, nx: f64, ny: f64) -> (i32, i32) {
    let nx = if nx.is_finite() {
        nx.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let ny = if ny.is_finite() {
        ny.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let px = monitor.x - layout.x + (nx * (monitor.w - 1) as f64).round() as i32;
    let py = monitor.y - layout.y + (ny * (monitor.h - 1) as f64).round() as i32;
    (px.clamp(0, layout.w - 1), py.clamp(0, layout.h - 1))
}

pub fn keycode(code: &str) -> Option<KeyCode> {
    KEYMAP.iter().find(|(c, _)| *c == code).map(|(_, k)| *k)
}

/// DOM `KeyboardEvent.code` (physical key position) → Linux keycode. Because codes are physical,
/// the PC's own keyboard layout decides which character is produced.
const KEYMAP: &[(&str, KeyCode)] = &[
    ("Escape", KeyCode::KEY_ESC),
    ("Digit1", KeyCode::KEY_1),
    ("Digit2", KeyCode::KEY_2),
    ("Digit3", KeyCode::KEY_3),
    ("Digit4", KeyCode::KEY_4),
    ("Digit5", KeyCode::KEY_5),
    ("Digit6", KeyCode::KEY_6),
    ("Digit7", KeyCode::KEY_7),
    ("Digit8", KeyCode::KEY_8),
    ("Digit9", KeyCode::KEY_9),
    ("Digit0", KeyCode::KEY_0),
    ("Minus", KeyCode::KEY_MINUS),
    ("Equal", KeyCode::KEY_EQUAL),
    ("Backspace", KeyCode::KEY_BACKSPACE),
    ("Tab", KeyCode::KEY_TAB),
    ("KeyQ", KeyCode::KEY_Q),
    ("KeyW", KeyCode::KEY_W),
    ("KeyE", KeyCode::KEY_E),
    ("KeyR", KeyCode::KEY_R),
    ("KeyT", KeyCode::KEY_T),
    ("KeyY", KeyCode::KEY_Y),
    ("KeyU", KeyCode::KEY_U),
    ("KeyI", KeyCode::KEY_I),
    ("KeyO", KeyCode::KEY_O),
    ("KeyP", KeyCode::KEY_P),
    ("BracketLeft", KeyCode::KEY_LEFTBRACE),
    ("BracketRight", KeyCode::KEY_RIGHTBRACE),
    ("Enter", KeyCode::KEY_ENTER),
    ("ControlLeft", KeyCode::KEY_LEFTCTRL),
    ("KeyA", KeyCode::KEY_A),
    ("KeyS", KeyCode::KEY_S),
    ("KeyD", KeyCode::KEY_D),
    ("KeyF", KeyCode::KEY_F),
    ("KeyG", KeyCode::KEY_G),
    ("KeyH", KeyCode::KEY_H),
    ("KeyJ", KeyCode::KEY_J),
    ("KeyK", KeyCode::KEY_K),
    ("KeyL", KeyCode::KEY_L),
    ("Semicolon", KeyCode::KEY_SEMICOLON),
    ("Quote", KeyCode::KEY_APOSTROPHE),
    ("Backquote", KeyCode::KEY_GRAVE),
    ("ShiftLeft", KeyCode::KEY_LEFTSHIFT),
    ("Backslash", KeyCode::KEY_BACKSLASH),
    ("KeyZ", KeyCode::KEY_Z),
    ("KeyX", KeyCode::KEY_X),
    ("KeyC", KeyCode::KEY_C),
    ("KeyV", KeyCode::KEY_V),
    ("KeyB", KeyCode::KEY_B),
    ("KeyN", KeyCode::KEY_N),
    ("KeyM", KeyCode::KEY_M),
    ("Comma", KeyCode::KEY_COMMA),
    ("Period", KeyCode::KEY_DOT),
    ("Slash", KeyCode::KEY_SLASH),
    ("ShiftRight", KeyCode::KEY_RIGHTSHIFT),
    ("NumpadMultiply", KeyCode::KEY_KPASTERISK),
    ("AltLeft", KeyCode::KEY_LEFTALT),
    ("Space", KeyCode::KEY_SPACE),
    ("CapsLock", KeyCode::KEY_CAPSLOCK),
    ("F1", KeyCode::KEY_F1),
    ("F2", KeyCode::KEY_F2),
    ("F3", KeyCode::KEY_F3),
    ("F4", KeyCode::KEY_F4),
    ("F5", KeyCode::KEY_F5),
    ("F6", KeyCode::KEY_F6),
    ("F7", KeyCode::KEY_F7),
    ("F8", KeyCode::KEY_F8),
    ("F9", KeyCode::KEY_F9),
    ("F10", KeyCode::KEY_F10),
    ("F11", KeyCode::KEY_F11),
    ("F12", KeyCode::KEY_F12),
    ("NumLock", KeyCode::KEY_NUMLOCK),
    ("ScrollLock", KeyCode::KEY_SCROLLLOCK),
    ("Numpad7", KeyCode::KEY_KP7),
    ("Numpad8", KeyCode::KEY_KP8),
    ("Numpad9", KeyCode::KEY_KP9),
    ("NumpadSubtract", KeyCode::KEY_KPMINUS),
    ("Numpad4", KeyCode::KEY_KP4),
    ("Numpad5", KeyCode::KEY_KP5),
    ("Numpad6", KeyCode::KEY_KP6),
    ("NumpadAdd", KeyCode::KEY_KPPLUS),
    ("Numpad1", KeyCode::KEY_KP1),
    ("Numpad2", KeyCode::KEY_KP2),
    ("Numpad3", KeyCode::KEY_KP3),
    ("Numpad0", KeyCode::KEY_KP0),
    ("NumpadDecimal", KeyCode::KEY_KPDOT),
    ("IntlBackslash", KeyCode::KEY_102ND),
    ("NumpadEnter", KeyCode::KEY_KPENTER),
    ("ControlRight", KeyCode::KEY_RIGHTCTRL),
    ("NumpadDivide", KeyCode::KEY_KPSLASH),
    ("PrintScreen", KeyCode::KEY_SYSRQ),
    ("AltRight", KeyCode::KEY_RIGHTALT),
    ("Home", KeyCode::KEY_HOME),
    ("ArrowUp", KeyCode::KEY_UP),
    ("PageUp", KeyCode::KEY_PAGEUP),
    ("ArrowLeft", KeyCode::KEY_LEFT),
    ("ArrowRight", KeyCode::KEY_RIGHT),
    ("End", KeyCode::KEY_END),
    ("ArrowDown", KeyCode::KEY_DOWN),
    ("PageDown", KeyCode::KEY_PAGEDOWN),
    ("Insert", KeyCode::KEY_INSERT),
    ("Delete", KeyCode::KEY_DELETE),
    ("Pause", KeyCode::KEY_PAUSE),
    ("MetaLeft", KeyCode::KEY_LEFTMETA),
    ("MetaRight", KeyCode::KEY_RIGHTMETA),
    ("ContextMenu", KeyCode::KEY_COMPOSE),
    ("NumpadEqual", KeyCode::KEY_KPEQUAL),
    ("AudioVolumeMute", KeyCode::KEY_MUTE),
    ("AudioVolumeDown", KeyCode::KEY_VOLUMEDOWN),
    ("AudioVolumeUp", KeyCode::KEY_VOLUMEUP),
    ("MediaPlayPause", KeyCode::KEY_PLAYPAUSE),
    ("MediaTrackNext", KeyCode::KEY_NEXTSONG),
    ("MediaTrackPrevious", KeyCode::KEY_PREVIOUSSONG),
];

#[cfg(test)]
mod tests {
    use super::*;

    const LAYOUT: Rect = Rect {
        x: 0,
        y: 0,
        w: 3840,
        h: 1080,
    };
    const RIGHT: Rect = Rect {
        x: 1920,
        y: 0,
        w: 1920,
        h: 1080,
    };

    #[test]
    fn maps_into_selected_monitor() {
        assert_eq!(map_point(LAYOUT, RIGHT, 0.0, 0.0), (1920, 0));
        assert_eq!(map_point(LAYOUT, RIGHT, 1.0, 1.0), (3839, 1079));
        assert_eq!(map_point(LAYOUT, RIGHT, 0.5, 0.5), (2880, 540));
        assert_eq!(map_point(LAYOUT, RIGHT, -3.0, f64::NAN), (1920, 0));
    }

    #[test]
    fn parses_messages() {
        let m: InputMsg = serde_json::from_str(r#"{"t":"k","c":"KeyA","d":true}"#).unwrap();
        assert_eq!(
            m,
            InputMsg::Key {
                c: "KeyA".into(),
                d: true
            }
        );
        let m: InputMsg = serde_json::from_str(r#"{"t":"m","x":0.25,"y":0.75}"#).unwrap();
        assert_eq!(m, InputMsg::Move { x: 0.25, y: 0.75 });
        assert!(serde_json::from_str::<InputMsg>(r#"{"t":"exec","cmd":"rm"}"#).is_err());
    }

    #[test]
    fn keymap_is_unique_and_complete() {
        let mut seen = HashSet::new();
        for (c, _) in KEYMAP {
            assert!(seen.insert(*c), "duplicate {c}");
        }
        for c in [
            "KeyA",
            "Enter",
            "ShiftLeft",
            "ArrowUp",
            "F12",
            "MetaLeft",
            "IntlBackslash",
        ] {
            assert!(keycode(c).is_some(), "{c}");
        }
        assert!(keycode("Nope").is_none());
    }
}
