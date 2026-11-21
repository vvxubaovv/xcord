//! Windows IME composition probing.
//!
//! GPUI normally sees composition through `marked_text_range`, but some IMEs
//! (notably Sogou) keep the candidate composition in IMM without exposing it to
//! the key-event path. In that case Enter/Backspace must be handed back to the
//! platform instead of being consumed by the text widget.

/// Ensure the window has an IMM32 input context. Some TSF-only IMEs switch to
/// IMM when a window explicitly associates a default context.
#[cfg(windows)]
pub fn ensure_imm_context(window: &gpui::Window) {
    use raw_window_handle::RawWindowHandle;
    use std::ffi::c_void;

    #[link(name = "imm32")]
    extern "system" {
        fn ImmAssociateContextEx(window: *mut c_void, context: *mut c_void, flags: u32) -> i32;
    }
    const IACE_DEFAULT: u32 = 0x0010;
    let Ok(handle) = raw_window_handle::HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    unsafe {
        ImmAssociateContextEx(
            handle.hwnd.get() as *mut c_void,
            std::ptr::null_mut(),
            IACE_DEFAULT,
        );
    }
}

#[cfg(not(windows))]
pub fn ensure_imm_context(_window: &gpui::Window) {}

#[cfg(windows)]
pub fn is_composing() -> bool {
    use std::ffi::c_void;

    #[link(name = "imm32")]
    extern "system" {
        fn ImmGetContext(window: *mut c_void) -> *mut c_void;
        fn ImmReleaseContext(window: *mut c_void, context: *mut c_void) -> i32;
        fn ImmGetCompositionStringW(
            context: *mut c_void,
            index: u32,
            buffer: *mut c_void,
            length: u32,
        ) -> i32;
    }
    #[link(name = "user32")]
    extern "system" {
        fn GetForegroundWindow() -> *mut c_void;
    }

    const GCS_COMPSTR: u32 = 0x0008;
    unsafe {
        let window = GetForegroundWindow();
        if window.is_null() {
            return false;
        }
        let context = ImmGetContext(window);
        if context.is_null() {
            return false;
        }
        let length = ImmGetCompositionStringW(context, GCS_COMPSTR, std::ptr::null_mut(), 0);
        ImmReleaseContext(window, context);
        length > 0
    }
}

#[cfg(not(windows))]
pub fn is_composing() -> bool {
    false
}
