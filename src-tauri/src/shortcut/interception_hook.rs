//! Windows Interception driver shortcut implementation.
//!
//! This backend uses the optional Interception driver/API when
//! `interception.dll` is available next to Handy or on PATH. It observes
//! keyboard scan codes below normal Win32 hooks, immediately sends each stroke
//! back unchanged, and uses the shared Windows shortcut tracker for matching.

use libloading::Library;
use log::{error, info};
use std::collections::HashMap;
use std::ffi::{c_int, c_ulong, c_void};
use std::mem::MaybeUninit;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tauri::{AppHandle, Manager};
use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VSC_TO_VK_EX};

use crate::settings::{self, get_settings, ShortcutBinding};

use super::handler::handle_shortcut_event;
use super::windows_hook::{
    register_binding, validate_shortcut as validate_windows_shortcut, RawKeyEvent,
    RegisteredShortcut, ShortcutTracker,
};

const INTERCEPTION_MAX_KEYBOARD: InterceptionDevice = 10;
const INTERCEPTION_FILTER_KEY_ALL: InterceptionFilter = 0xffff;
const INTERCEPTION_KEY_UP: u16 = 0x01;
const INTERCEPTION_KEY_E0: u16 = 0x02;
const INTERCEPTION_KEY_E1: u16 = 0x04;

type InterceptionContext = *mut c_void;
type InterceptionDevice = c_int;
type InterceptionFilter = u16;
type InterceptionPredicate = unsafe extern "C" fn(InterceptionDevice) -> c_int;

type InterceptionCreateContext = unsafe extern "C" fn() -> InterceptionContext;
type InterceptionDestroyContext = unsafe extern "C" fn(InterceptionContext);
type InterceptionSetFilter =
    unsafe extern "C" fn(InterceptionContext, InterceptionPredicate, InterceptionFilter);
type InterceptionWaitWithTimeout =
    unsafe extern "C" fn(InterceptionContext, c_ulong) -> InterceptionDevice;
type InterceptionSend = unsafe extern "C" fn(
    InterceptionContext,
    InterceptionDevice,
    *const InterceptionStroke,
    u32,
) -> c_int;
type InterceptionReceive = unsafe extern "C" fn(
    InterceptionContext,
    InterceptionDevice,
    *mut InterceptionStroke,
    u32,
) -> c_int;
type InterceptionIsInvalid = unsafe extern "C" fn(InterceptionDevice) -> c_int;
type InterceptionIsKeyboard = unsafe extern "C" fn(InterceptionDevice) -> c_int;

#[repr(C)]
#[derive(Clone, Copy)]
struct InterceptionMouseStroke {
    state: u16,
    flags: u16,
    rolling: i16,
    x: i32,
    y: i32,
    information: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InterceptionKeyStroke {
    code: u16,
    state: u16,
    information: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
union InterceptionStroke {
    mouse: InterceptionMouseStroke,
    key: InterceptionKeyStroke,
}

struct InterceptionApi {
    _library: Library,
    create_context: InterceptionCreateContext,
    destroy_context: InterceptionDestroyContext,
    set_filter: InterceptionSetFilter,
    wait_with_timeout: InterceptionWaitWithTimeout,
    send: InterceptionSend,
    receive: InterceptionReceive,
    is_invalid: InterceptionIsInvalid,
    is_keyboard: InterceptionIsKeyboard,
}

impl InterceptionApi {
    fn load() -> Result<Self, String> {
        let library = unsafe { Library::new("interception.dll") }.map_err(|err| {
            format!(
                "Failed to load interception.dll: {err}. Put the x64 Interception DLL next to handy.exe and install the Interception driver as Administrator."
            )
        })?;

        unsafe {
            Ok(Self {
                create_context: *library
                    .get(b"interception_create_context\0")
                    .map_err(|err| format!("interception_create_context missing: {err}"))?,
                destroy_context: *library
                    .get(b"interception_destroy_context\0")
                    .map_err(|err| format!("interception_destroy_context missing: {err}"))?,
                set_filter: *library
                    .get(b"interception_set_filter\0")
                    .map_err(|err| format!("interception_set_filter missing: {err}"))?,
                wait_with_timeout: *library
                    .get(b"interception_wait_with_timeout\0")
                    .map_err(|err| format!("interception_wait_with_timeout missing: {err}"))?,
                send: *library
                    .get(b"interception_send\0")
                    .map_err(|err| format!("interception_send missing: {err}"))?,
                receive: *library
                    .get(b"interception_receive\0")
                    .map_err(|err| format!("interception_receive missing: {err}"))?,
                is_invalid: *library
                    .get(b"interception_is_invalid\0")
                    .map_err(|err| format!("interception_is_invalid missing: {err}"))?,
                is_keyboard: *library
                    .get(b"interception_is_keyboard\0")
                    .map_err(|err| format!("interception_is_keyboard missing: {err}"))?,
                _library: library,
            })
        }
    }
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

pub struct InterceptionHookState {
    runtime: Mutex<Option<InterceptionHookRuntime>>,
}

struct InterceptionHookRuntime {
    command_sender: Sender<ManagerCommand>,
    thread_handle: JoinHandle<()>,
}

impl InterceptionHookState {
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
            .map_err(|_| "Failed to lock Interception hook runtime")?;

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
                *runtime = Some(InterceptionHookRuntime {
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
                Err(format!("Timed out starting Interception hook: {err}"))
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
        let api = match InterceptionApi::load() {
            Ok(api) => api,
            Err(err) => {
                let _ = ready_tx.send(Err(err));
                return;
            }
        };

        let context = unsafe { (api.create_context)() };
        if context.is_null() {
            let _ = ready_tx.send(Err(
                "Failed to create Interception context. Is the Interception driver installed?"
                    .into(),
            ));
            return;
        }

        unsafe {
            (api.set_filter)(
                context,
                interception_keyboard_predicate,
                INTERCEPTION_FILTER_KEY_ALL,
            );
        }

        let _ = ready_tx.send(Ok(()));
        info!("Windows Interception shortcut hook started");

        let mut bindings: HashMap<String, RegisteredShortcut> = HashMap::new();
        let mut tracker = ShortcutTracker::default();
        let mut running = true;

        while running {
            while let Ok(command) = cmd_rx.try_recv() {
                match command {
                    ManagerCommand::Register {
                        binding_id,
                        hotkey_string,
                        response,
                    } => {
                        let result = register_binding(
                            &mut bindings,
                            binding_id,
                            hotkey_string,
                            "Windows Interception",
                        );
                        let _ = response.send(result);
                    }
                    ManagerCommand::Unregister {
                        binding_id,
                        response,
                    } => {
                        bindings.remove(&binding_id);
                        tracker.remove_binding(&binding_id);
                        let _ = response.send(Ok(()));
                    }
                    ManagerCommand::Shutdown => {
                        running = false;
                    }
                }
            }

            if !running {
                break;
            }

            let device = unsafe { (api.wait_with_timeout)(context, 10) };
            if device == 0
                || unsafe { (api.is_invalid)(device) != 0 }
                || unsafe { (api.is_keyboard)(device) == 0 }
            {
                continue;
            }

            let mut stroke = MaybeUninit::<InterceptionStroke>::uninit();
            let received = unsafe { (api.receive)(context, device, stroke.as_mut_ptr(), 1) };
            if received <= 0 {
                continue;
            }

            let stroke = unsafe { stroke.assume_init() };
            let key_stroke = unsafe { stroke.key };
            let events = key_stroke
                .to_raw_key_event()
                .map(|event| tracker.apply(event, &bindings))
                .unwrap_or_default();

            let sent = unsafe { (api.send)(context, device, &stroke, 1) };
            if sent != 1 {
                error!("Failed to pass Interception keyboard stroke through");
            }

            for (shortcut, is_pressed) in events {
                handle_shortcut_event(
                    &app,
                    &shortcut.binding_id,
                    &shortcut.hotkey_string,
                    is_pressed,
                );
            }
        }

        unsafe {
            (api.destroy_context)(context);
        }
        info!("Windows Interception shortcut hook stopped");
    }

    pub fn register(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let sender = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock Interception hook runtime")?
            .as_ref()
            .map(|runtime| runtime.command_sender.clone())
            .ok_or_else(|| "Interception hook is not running".to_string())?;

        sender
            .send(ManagerCommand::Register {
                binding_id: binding.id.clone(),
                hotkey_string: binding.current_binding.clone(),
                response: tx,
            })
            .map_err(|_| "Failed to send Interception register command")?;

        rx.recv()
            .map_err(|_| "Failed to receive Interception register response")?
    }

    pub fn unregister(&self, binding: &ShortcutBinding) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        let sender = self
            .runtime
            .lock()
            .map_err(|_| "Failed to lock Interception hook runtime")?
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
            .map_err(|_| "Failed to send Interception unregister command")?;

        rx.recv()
            .map_err(|_| "Failed to receive Interception unregister response")?
    }
}

impl Drop for InterceptionHookState {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl InterceptionKeyStroke {
    fn to_raw_key_event(self) -> Option<RawKeyEvent> {
        let mut scan_code = self.code as u32;
        if self.state & INTERCEPTION_KEY_E0 != 0 {
            scan_code |= 0xe000;
        } else if self.state & INTERCEPTION_KEY_E1 != 0 {
            scan_code |= 0xe100;
        }

        let vk_code = unsafe { MapVirtualKeyW(scan_code, MAPVK_VSC_TO_VK_EX) };
        if vk_code == 0 {
            return None;
        }

        Some(RawKeyEvent {
            vk_code,
            is_down: self.state & INTERCEPTION_KEY_UP == 0,
        })
    }
}

unsafe extern "C" fn interception_keyboard_predicate(device: InterceptionDevice) -> c_int {
    (device >= 1 && device <= INTERCEPTION_MAX_KEYBOARD) as c_int
}

pub fn validate_shortcut(raw: &str) -> Result<(), String> {
    validate_windows_shortcut(raw)
}

pub fn init_state(app: &AppHandle) -> Result<(), String> {
    if let Some(state) = app.try_state::<InterceptionHookState>() {
        state.start(app.clone())?;
        return Ok(());
    }

    let state = InterceptionHookState::new(app.clone())?;
    app.manage(state);
    info!("Windows Interception shortcut hook initialized");
    Ok(())
}

pub fn shutdown(app: &AppHandle) {
    if let Some(state) = app.try_state::<InterceptionHookState>() {
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
                "Failed to register Windows Interception shortcut {} during init: {}",
                id, err
            );
        }
    }

    Ok(())
}

pub fn register_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<InterceptionHookState>()
        .ok_or_else(|| "Windows Interception hook state is not initialized".to_string())?;

    state.register(&binding)
}

pub fn unregister_shortcut(app: &AppHandle, binding: ShortcutBinding) -> Result<(), String> {
    let state = app
        .try_state::<InterceptionHookState>()
        .ok_or_else(|| "Windows Interception hook state is not initialized".to_string())?;

    state.unregister(&binding)
}

pub fn register_cancel_shortcut(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(cancel_binding) = get_settings(&app).bindings.get("cancel").cloned() {
            if let Err(err) = register_shortcut(&app, cancel_binding) {
                error!("Failed to register Windows Interception cancel shortcut: {err}");
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

    #[test]
    fn maps_basic_interception_scan_code_to_vk() {
        let event = InterceptionKeyStroke {
            code: 0x39,
            state: 0,
            information: 0,
        }
        .to_raw_key_event()
        .unwrap();

        assert_eq!(event.vk_code, 0x20);
        assert!(event.is_down);
    }

    #[test]
    fn maps_key_up_state() {
        let event = InterceptionKeyStroke {
            code: 0x39,
            state: INTERCEPTION_KEY_UP,
            information: 0,
        }
        .to_raw_key_event()
        .unwrap();

        assert!(!event.is_down);
    }
}
