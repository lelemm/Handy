//! Windows low-level keyboard hook shortcut implementation.
//!
//! This backend observes host keyboard input with `WH_KEYBOARD_LL` and always
//! forwards events to the next hook. That lets (not)Handy react while focused apps
//! like RDP still receive the same shortcut.

use log::{debug, error, info};
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, PeekMessageW, SetWindowsHookExW, TranslateMessage,
    UnhookWindowsHookEx, HC_ACTION, KBDLLHOOKSTRUCT, MSG, PM_REMOVE, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;

const VK_BACK: u32 = 0x08;
const VK_TAB: u32 = 0x09;
const VK_RETURN: u32 = 0x0d;
const VK_SHIFT: u32 = 0x10;
const VK_CONTROL: u32 = 0x11;
const VK_MENU: u32 = 0x12;
const VK_PAUSE: u32 = 0x13;
const VK_CAPITAL: u32 = 0x14;
const VK_ESCAPE: u32 = 0x1b;
const VK_SPACE: u32 = 0x20;
const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_END: u32 = 0x23;
const VK_HOME: u32 = 0x24;
const VK_LEFT: u32 = 0x25;
const VK_UP: u32 = 0x26;
const VK_RIGHT: u32 = 0x27;
const VK_DOWN: u32 = 0x28;
const VK_SNAPSHOT: u32 = 0x2c;
const VK_INSERT: u32 = 0x2d;
const VK_DELETE: u32 = 0x2e;
const VK_LWIN: u32 = 0x5b;
const VK_RWIN: u32 = 0x5c;
const VK_NUMPAD0: u32 = 0x60;
const VK_MULTIPLY: u32 = 0x6a;
const VK_ADD: u32 = 0x6b;
const VK_SUBTRACT: u32 = 0x6d;
const VK_DECIMAL: u32 = 0x6e;
const VK_DIVIDE: u32 = 0x6f;
const VK_F1: u32 = 0x70;
const VK_NUMLOCK: u32 = 0x90;
const VK_SCROLL: u32 = 0x91;
const VK_LSHIFT: u32 = 0xa0;
const VK_RSHIFT: u32 = 0xa1;
const VK_LCONTROL: u32 = 0xa2;
const VK_RCONTROL: u32 = 0xa3;
const VK_LMENU: u32 = 0xa4;
const VK_RMENU: u32 = 0xa5;
const VK_OEM_1: u32 = 0xba;
const VK_OEM_PLUS: u32 = 0xbb;
const VK_OEM_COMMA: u32 = 0xbc;
const VK_OEM_MINUS: u32 = 0xbd;
const VK_OEM_PERIOD: u32 = 0xbe;
const VK_OEM_2: u32 = 0xbf;
const VK_OEM_3: u32 = 0xc0;
const VK_OEM_4: u32 = 0xdb;
const VK_OEM_5: u32 = 0xdc;
const VK_OEM_6: u32 = 0xdd;
const VK_OEM_7: u32 = 0xde;

#[derive(Clone, Debug)]
pub(crate) struct RegisteredShortcut {
    pub(crate) binding_id: String,
    pub(crate) hotkey_string: String,
    parsed: ParsedShortcut,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedShortcut {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
    key: Option<u32>,
}

#[derive(Clone, Copy)]
pub(crate) struct RawKeyEvent {
    pub(crate) vk_code: u32,
    pub(crate) is_down: bool,
}

enum ManagerCommand {
    Register {
        binding_id: String,
        hotkey_string: String,
        response: Sender<Result<(), String>>,
    },
    Unregister {
        binding_id: String,
        response: Sender<Result<(), String>>,
    },
    Shutdown,
}

pub struct WindowsHookState {
    runtime: Mutex<Option<WindowsHookRuntime>>,
}

struct WindowsHookRuntime {
    command_sender: Sender<ManagerCommand>,
    thread_handle: JoinHandle<()>,
}

static RAW_KEY_SENDER: Lazy<Mutex<Option<Sender<RawKeyEvent>>>> = Lazy::new(|| Mutex::new(None));

impl WindowsHookState {
    pub fn new(app: AppHandle) -> Result<Self, String> {
        let state = Self {
            runtime: Mutex::new(None),
        };
        state.start(app)?;
        Ok(state)
    }

    pub fn start(&self, app: AppHandle) -> Result<(), String> {
        let mut runtime = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock Windows hook runtime")?;

        if runtime.is_some() {
            return Ok(());
        }

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();

        let thread_handle = thread::spawn(move || {
            Self::manager_thread(app, cmd_rx, ready_tx);
        });

        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {
                *runtime = Some(WindowsHookRuntime {
                    command_sender: cmd_tx,
                    thread_handle,
                });
                Ok(())
            }
            Ok(Err(err)) => {
                let _ = thread_handle.join();
                Err(err)
            }
            Err(err) => {
                let _ = thread_handle.join();
                Err(format!("Timed out starting Windows low-level hook: {err}"))
            }
        }
    }

    pub fn shutdown(&self) {
        let runtime = self.runtime.lock().ok().and_then(|mut guard| guard.take());

        if let Some(runtime) = runtime {
            let _ = runtime.command_sender.send(ManagerCommand::Shutdown);
            let _ = runtime.thread_handle.join();
        }
    }

    fn manager_thread(
        app: AppHandle,
        cmd_rx: Receiver<ManagerCommand>,
        ready_tx: Sender<Result<(), String>>,
    ) {
        let (raw_tx, raw_rx) = mpsc::channel();
        {
            let mut sender = RAW_KEY_SENDER.lock().unwrap();
            *sender = Some(raw_tx);
        }

        let hook = match unsafe {
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_keyboard_proc), None, 0)
        } {
            Ok(hook) => {
                let _ = ready_tx.send(Ok(()));
                hook
            }
            Err(err) => {
                let mut sender = RAW_KEY_SENDER.lock().unwrap();
                *sender = None;
                let _ = ready_tx.send(Err(format!(
                    "Failed to install Windows low-level keyboard hook: {err}"
                )));
                return;
            }
        };

        info!("Windows low-level shortcut hook started");

        let mut bindings: HashMap<String, RegisteredShortcut> = HashMap::new();
        let mut tracker = ShortcutTracker::default();
        let mut running = true;

        while running {
            pump_messages();

            while let Ok(event) = raw_rx.try_recv() {
                for (shortcut, is_pressed) in tracker.apply(event, &bindings) {
                    handle_shortcut_event(
                        &app,
                        &shortcut.binding_id,
                        &shortcut.hotkey_string,
                        is_pressed,
                    );
                }
            }

            match cmd_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(ManagerCommand::Register {
                    binding_id,
                    hotkey_string,
                    response,
                }) => {
                    let result = register_binding(
                        &mut bindings,
                        binding_id,
                        hotkey_string,
                        "Windows low-level",
                    );
                    let _ = response.send(result);
                }
                Ok(ManagerCommand::Unregister {
                    binding_id,
                    response,
                }) => {
                    bindings.remove(&binding_id);
                    tracker.remove_binding(&binding_id);
                    let _ = response.send(Ok(()));
                }
                Ok(ManagerCommand::Shutdown) => {
                    running = false;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    running = false;
                }
            }
        }

        unsafe {
            if let Err(err) = UnhookWindowsHookEx(hook) {
                error!("Failed to uninstall Windows low-level keyboard hook: {err}");
            }
        }

        let mut sender = RAW_KEY_SENDER.lock().unwrap();
        *sender = None;
        info!("Windows low-level shortcut hook stopped");
    }

    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let sender = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock Windows hook runtime")?
            .as_ref()
            .map(|runtime| runtime.command_sender.clone())
            .ok_or_else(|| "Windows low-level hook is not running".to_string())?;

        sender
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send Windows hook register command")?;

        rx.recv()
            .map_err(|_| "Failed to receive Windows hook register response")?
    }

    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let sender = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock Windows hook runtime")?
            .as_ref()
            .map(|runtime| runtime.command_sender.clone());

        let Some(sender) = sender else {
            return Ok(());
        };

        sender
            .send(ManagerCommand::Unregister {
                binding_id: binding.id.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send Windows hook unregister command")?;

        rx.recv()
            .map_err(|_| "Failed to receive Windows hook unregister response")?
    }
}

#[derive(Default)]
pub(crate) struct ShortcutTracker {
    pressed_keys: HashSet<u32>,
    active_bindings: HashSet<String>,
}

impl ShortcutTracker {
    pub(crate) fn apply(
        &mut self,
        event: RawKeyEvent,
        bindings: &HashMap<String, RegisteredShortcut>,
    ) -> Vec<(RegisteredShortcut, bool)> {
        if event.is_down {
            self.pressed_keys.insert(event.vk_code);

            let mut fired = Vec::new();
            for shortcut in bindings.values() {
                if !self.active_bindings.contains(&shortcut.binding_id)
                    && shortcut.parsed.matches(&self.pressed_keys)
                {
                    self.active_bindings.insert(shortcut.binding_id.clone());
                    fired.push((shortcut.clone(), true));
                }
            }

            return fired;
        }

        self.pressed_keys.remove(&event.vk_code);

        let released = self
            .active_bindings
            .iter()
            .filter_map(|binding_id| {
                let shortcut = bindings.get(binding_id)?;
                (!shortcut.parsed.matches(&self.pressed_keys)).then_some(shortcut.clone())
            })
            .collect::<Vec<_>>();

        for shortcut in &released {
            self.active_bindings.remove(&shortcut.binding_id);
        }

        released
            .into_iter()
            .map(|shortcut| (shortcut, false))
            .collect()
    }

    pub(crate) fn remove_binding(&mut self, binding_id: &str) {
        self.active_bindings.remove(binding_id);
    }
}

impl Drop for WindowsHookState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

unsafe extern "system" fn low_level_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code == HC_ACTION as i32 {
        let message = wparam.0 as u32;
        let is_down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        let is_up = matches!(message, WM_KEYUP | WM_SYSKEYUP);

        if is_down || is_up {
            let keyboard = unsafe { *(lparam.0 as *const KBDLLHOOKSTRUCT) };
            let sender = RAW_KEY_SENDER.lock().unwrap().clone();

            if let Some(sender) = sender {
                let _ = sender.send(RawKeyEvent {
                    vk_code: keyboard.vkCode,
                    is_down,
                });
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn pump_messages() {
    let mut msg = MSG::default();

    while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() } {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

impl ParsedShortcut {
    fn matches(&self, pressed_keys: &HashSet<u32>) -> bool {
        self.ctrl == any_pressed(pressed_keys, &[VK_CONTROL, VK_LCONTROL, VK_RCONTROL])
            && self.alt == any_pressed(pressed_keys, &[VK_MENU, VK_LMENU, VK_RMENU])
            && self.shift == any_pressed(pressed_keys, &[VK_SHIFT, VK_LSHIFT, VK_RSHIFT])
            && self.meta == any_pressed(pressed_keys, &[VK_LWIN, VK_RWIN])
            && self
                .key
                .map(|key| key_pressed(pressed_keys, key))
                .unwrap_or(true)
    }
}

fn any_pressed(pressed_keys: &HashSet<u32>, keys: &[u32]) -> bool {
    keys.iter().any(|key| pressed_keys.contains(key))
}

fn key_pressed(pressed_keys: &HashSet<u32>, key: u32) -> bool {
    if key == VK_CONTROL {
        return any_pressed(pressed_keys, &[VK_CONTROL, VK_LCONTROL, VK_RCONTROL]);
    }
    if key == VK_MENU {
        return any_pressed(pressed_keys, &[VK_MENU, VK_LMENU, VK_RMENU]);
    }
    if key == VK_SHIFT {
        return any_pressed(pressed_keys, &[VK_SHIFT, VK_LSHIFT, VK_RSHIFT]);
    }

    pressed_keys.contains(&key)
}

pub(crate) fn parse_shortcut(raw: &str) -> Result<ParsedShortcut, String> {
    let mut shortcut = ParsedShortcut {
        ctrl: false,
        alt: false,
        shift: false,
        meta: false,
        key: None,
    };

    for part in split_shortcut_parts(raw)
        .into_iter()
        .map(|part| normalize_part(part))
        .filter(|part| !part.is_empty())
    {
        match part.as_str() {
            "ctrl" | "control" => shortcut.ctrl = true,
            "alt" | "option" => shortcut.alt = true,
            "shift" => shortcut.shift = true,
            "cmd" | "command" | "super" | "win" | "windows" | "meta" => shortcut.meta = true,
            "fn" | "function" => {
                return Err("The fn key is not available to the Windows low-level hook".into())
            }
            key => {
                if shortcut.key.is_some() {
                    return Err(format!("Only one non-modifier key is supported: '{raw}'"));
                }
                shortcut.key = Some(parse_key(key)?);
            }
        }
    }

    if !shortcut.ctrl
        && !shortcut.alt
        && !shortcut.shift
        && !shortcut.meta
        && shortcut.key.is_none()
    {
        return Err("Shortcut cannot be empty".into());
    }

    Ok(shortcut)
}

fn split_shortcut_parts(raw: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut last_separator_end = 0;

    for (index, ch) in raw.char_indices() {
        if ch != '+' {
            continue;
        }

        let current_part = &raw[last_separator_end..index];
        if current_part.trim_end().eq_ignore_ascii_case("numpad") {
            continue;
        }

        parts.push(&raw[start..index]);
        start = index + ch.len_utf8();
        last_separator_end = start;
    }

    parts.push(&raw[start..]);
    parts
}

fn normalize_part(part: &str) -> String {
    let part = part.trim().to_ascii_lowercase().replace('_', " ");
    let without_prefix = part
        .strip_prefix("left ")
        .or_else(|| part.strip_prefix("right "))
        .unwrap_or(&part);

    without_prefix
        .strip_suffix(" left")
        .or_else(|| without_prefix.strip_suffix(" right"))
        .unwrap_or(without_prefix)
        .to_string()
}

fn parse_key(key: &str) -> Result<u32, String> {
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Ok(ch.to_ascii_uppercase() as u32);
        }
        if ch.is_ascii_digit() {
            return Ok(ch as u32);
        }
    }

    if let Some(number) = key.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&number) {
            return Ok(VK_F1 + number - 1);
        }
    }

    if let Some(number) = key
        .strip_prefix("numpad ")
        .and_then(|n| n.parse::<u32>().ok())
    {
        if number <= 9 {
            return Ok(VK_NUMPAD0 + number);
        }
    }

    match key {
        "backspace" => Ok(VK_BACK),
        "tab" => Ok(VK_TAB),
        "enter" | "return" => Ok(VK_RETURN),
        "pause" => Ok(VK_PAUSE),
        "caps lock" | "capslock" => Ok(VK_CAPITAL),
        "escape" | "esc" => Ok(VK_ESCAPE),
        "space" => Ok(VK_SPACE),
        "page up" | "pageup" => Ok(VK_PRIOR),
        "page down" | "pagedown" => Ok(VK_NEXT),
        "end" => Ok(VK_END),
        "home" => Ok(VK_HOME),
        "left" | "arrow left" => Ok(VK_LEFT),
        "up" | "arrow up" => Ok(VK_UP),
        "right" | "arrow right" => Ok(VK_RIGHT),
        "down" | "arrow down" => Ok(VK_DOWN),
        "print screen" | "printscreen" => Ok(VK_SNAPSHOT),
        "insert" => Ok(VK_INSERT),
        "delete" | "del" => Ok(VK_DELETE),
        "menu" => Ok(0x5d),
        "num lock" | "numlock" => Ok(VK_NUMLOCK),
        "scroll lock" | "scrolllock" => Ok(VK_SCROLL),
        "numpad *" => Ok(VK_MULTIPLY),
        "numpad +" => Ok(VK_ADD),
        "numpad -" => Ok(VK_SUBTRACT),
        "numpad ." => Ok(VK_DECIMAL),
        "numpad /" => Ok(VK_DIVIDE),
        ";" => Ok(VK_OEM_1),
        "=" => Ok(VK_OEM_PLUS),
        "," => Ok(VK_OEM_COMMA),
        "-" => Ok(VK_OEM_MINUS),
        "." => Ok(VK_OEM_PERIOD),
        "/" => Ok(VK_OEM_2),
        "`" => Ok(VK_OEM_3),
        "[" => Ok(VK_OEM_4),
        "\\" => Ok(VK_OEM_5),
        "]" => Ok(VK_OEM_6),
        "'" => Ok(VK_OEM_7),
        _ => Err(format!("Unsupported Windows low-level hook key: '{key}'")),
    }
}

pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    parse_shortcut(raw).map(|_| ())
}

pub(crate) fn register_binding(
    bindings: &mut HashMap<String, RegisteredShortcut>,
    binding_id: String,
    hotkey_string: String,
    backend_name: &str,
) -> Result<(), String> {
    let parsed = parse_shortcut(&hotkey_string)?;

    if let Some(existing) = bindings
        .values()
        .find(|binding| binding.parsed == parsed && binding.binding_id != binding_id)
    {
        return Err(format!(
            "Shortcut '{}' is already in use by '{}'",
            hotkey_string, existing.binding_id
        ));
    }

    debug!(
        "Registered {} shortcut: {} -> {}",
        backend_name, binding_id, hotkey_string
    );

    bindings.insert(
        binding_id.clone(),
        RegisteredShortcut {
            binding_id,
            hotkey_string,
            parsed,
        },
    );

    Ok(())
}

pub fn init_state(app: &AppHandle) -> Result<(), String> {
    if let Some(state) = app.try_state::<WindowsHookState>() {
        state.start(app.clone())?;
        return Ok(());
    }

    let state = WindowsHookState::new(app.clone())?;
    app.manage(state);
    info!("Windows low-level shortcut hook initialized");
    Ok(())
}

pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<WindowsHookState>() {
        state.shutdown();
    }
}

pub fn init_shortcuts(app: &AppHandle) -> Result<(), String> {
    init_state(app)?;

    let default_bindings = settings::get_default_settings().bindings;
    let user_settings = settings::load_or_create_app_settings(app);

    for (id, default_binding) in default_bindings {
        if id == "cancel" {
            continue;
        }
        if id == "transcribe_with_post_process" && !user_settings.post_process_enabled {
            continue;
        }

        let binding = user_settings
            .bindings
            .get(&id)
            .cloned()
            .unwrap_or(default_binding);

        if let Err(err) = register_shortcut(app, binding) {
            error!(
                "Failed to register Windows low-level shortcut {} during init: {}",
                id, err
            );
        }
    }

    Ok(())
}

pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<WindowsHookState>()
        .ok_or_else(|| "Windows low-level hook state is not initialized".to_string())?;

    state.register(&binding)
}

pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<WindowsHookState>()
        .ok_or_else(|| "Windows low-level hook state is not initialized".to_string())?;

    state.unregister(&binding)
}

pub fn register_cancel_shortcut(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(cancel_binding) = get_settings(&app).bindings.get("cancel").cloned() {
            if let Err(err) = register_shortcut(&app, cancel_binding) {
                error!("Failed to register Windows low-level cancel shortcut: {err}");
            }
        }
    });
}

pub fn unregister_cancel_shortcut(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(cancel_binding) = get_settings(&app).bindings.get("cancel").cloned() {
            let _ = unregister_shortcut(&app, cancel_binding);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(binding_id: &str, hotkey_string: &str) -> RegisteredShortcut {
        RegisteredShortcut {
            binding_id: binding_id.to_string(),
            hotkey_string: hotkey_string.to_string(),
            parsed: parse_shortcut(hotkey_string).unwrap(),
        }
    }

    fn bindings(shortcuts: &[RegisteredShortcut]) -> HashMap<String, RegisteredShortcut> {
        shortcuts
            .iter()
            .map(|shortcut| (shortcut.binding_id.clone(), shortcut.clone()))
            .collect()
    }

    fn down(vk_code: u32) -> RawKeyEvent {
        RawKeyEvent {
            vk_code,
            is_down: true,
        }
    }

    fn up(vk_code: u32) -> RawKeyEvent {
        RawKeyEvent {
            vk_code,
            is_down: false,
        }
    }

    #[test]
    fn parses_common_windows_shortcuts() {
        let parsed = parse_shortcut("ctrl+shift+space").unwrap();

        assert!(parsed.ctrl);
        assert!(parsed.shift);
        assert!(!parsed.alt);
        assert!(!parsed.meta);
        assert_eq!(parsed.key, Some(VK_SPACE));
    }

    #[test]
    fn rejects_fn_key() {
        let err = parse_shortcut("fn+space").unwrap_err();

        assert!(err.contains("fn key"));
    }

    #[test]
    fn parses_numpad_plus_without_splitting_it_as_separator() {
        let parsed = parse_shortcut("ctrl+numpad +").unwrap();

        assert!(parsed.ctrl);
        assert_eq!(parsed.key, Some(VK_ADD));
    }

    #[test]
    fn matches_exact_modifier_set() {
        let ctrl_space = parse_shortcut("ctrl+space").unwrap();
        let mut pressed = HashSet::from([VK_LCONTROL, VK_SPACE]);

        assert!(ctrl_space.matches(&pressed));

        pressed.insert(VK_SHIFT);
        assert!(!ctrl_space.matches(&pressed));
    }

    #[test]
    fn fires_once_for_held_chord_and_releases_on_key_up() {
        let shortcuts = bindings(&[shortcut("transcribe", "ctrl+space")]);
        let mut tracker = ShortcutTracker::default();

        assert!(tracker.apply(down(VK_LCONTROL), &shortcuts).is_empty());

        let pressed = tracker.apply(down(VK_SPACE), &shortcuts);
        assert_eq!(pressed.len(), 1);
        assert_eq!(pressed[0].0.binding_id, "transcribe");
        assert!(pressed[0].1);

        assert!(tracker.apply(down(VK_SPACE), &shortcuts).is_empty());

        let released = tracker.apply(up(VK_SPACE), &shortcuts);
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].0.binding_id, "transcribe");
        assert!(!released[0].1);
    }

    #[test]
    fn unregistering_active_binding_prevents_late_release() {
        let transcribe = shortcut("transcribe", "ctrl+space");
        let shortcuts = bindings(&[transcribe]);
        let mut tracker = ShortcutTracker::default();

        tracker.apply(down(VK_LCONTROL), &shortcuts);
        assert_eq!(tracker.apply(down(VK_SPACE), &shortcuts).len(), 1);

        tracker.remove_binding("transcribe");
        assert!(tracker.apply(up(VK_SPACE), &shortcuts).is_empty());
    }
}
