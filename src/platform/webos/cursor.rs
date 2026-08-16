//! Two layers can draw a pointer: SDL's own cursor (`show_cursor`) and the compositor's
//! (`SDL_webOSCursorVisibility` → `wl_webos_input_manager.set_cursor_visibility`).
//!
//! **Do not call the compositor visibility API.** On webOS 26 a hide is permanent for this
//! Wayland connection — a later show does not restore the arrow, and only restarting the
//! client does. Capture-on hides the TV pointer with `evmouse`'s `EVIOCGRAB` (starves
//! surface-manager of reports) plus SDL `show_cursor(false)`. Capture-off / menu: ungrab and
//! `show_cursor(true)`. The FFI is kept in comments in `docs/NOTES.md`, not here.

use sdl2::mouse::MouseUtil;

/// The local pointer's SDL visibility and capture state. Drive from the SDL video thread.
pub struct Cursor {
    mouse: MouseUtil,
    captured: bool,
    sdl_relative: bool,
}

impl Cursor {
    pub fn new(mouse: MouseUtil) -> Self {
        Self {
            mouse,
            captured: false,
            sdl_relative: true,
        }
    }

    /// Stop asking SDL for relative mode, for when motion is read via `super::evmouse` instead:
    /// the fork emulates relative mode with a screen-centre warp per motion event, which is
    /// pure waste for a source we don't read. aurora-tv does the same under `hardware_mouse`.
    ///
    /// Must run **before** [`Self::set_captured`]`(true)` when a HID mouse is expected: capture
    /// otherwise enables relative mode for the HID scan window, parks SDL at screen centre,
    /// and the next Capture-off stream shows the TV cursor at a constant offset.
    pub fn disable_sdl_relative(&mut self) {
        self.sdl_relative = false;
        self.apply();
    }

    /// Capture-on fallback when no HID mouse shows up — Magic Remote needs unbounded deltas.
    pub fn enable_sdl_relative(&mut self) {
        self.sdl_relative = true;
        self.apply();
    }

    /// Capture the pointer for the host — SDL cursor hidden, and SDL switched to relative mode
    /// so motion arrives as unbounded deltas instead of coordinates that stop at the panel edge.
    /// The compositor arrow is hidden by grabbing the HID mouse, not by a visibility ioctl.
    /// Uncaptured is the menu/desktop state: SDL cursor visible, absolute.
    pub fn set_captured(&mut self, captured: bool) {
        self.captured = captured;
        self.apply();
    }

    pub fn is_captured(&self) -> bool {
        self.captured
    }

    /// Put SDL's pointer on `(x, y)`. Used after Capture-on so the next desktop/menu
    /// session does not inherit the relative-mode centre warp.
    pub fn warp_abs(&self, window: &sdl2::video::Window, x: i32, y: i32) {
        self.mouse.warp_mouse_in_window(window, x, y);
    }

    fn apply(&mut self) {
        self.mouse.show_cursor(!self.captured);
        self.mouse.set_relative_mouse_mode(self.captured && self.sdl_relative);
    }

    /// SDL hide again once the evdev grab has actually landed. No-op while uncaptured.
    pub fn reassert_hidden(&mut self) {
        if !self.captured {
            return;
        }
        self.mouse.show_cursor(false);
    }

    /// Reserved for a compositor hide-loop that must not run on webOS 26 (sticky hide).
    pub fn on_pointer_activity(&mut self) {}
}

/// Panic-hook stand-in. Compositor visibility is not touched (sticky hide on this firmware).
pub fn restore_on_exit() {}
