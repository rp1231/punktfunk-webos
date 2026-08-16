//! Two layers can draw a pointer: SDL's own cursor (`show_cursor`) and the compositor's
//! (luna-surfacemanager / LSM).
//!
//! **Do not call `SDL_webOSCursorVisibility`.** On webOS 26 a hide via
//! `wl_webos_input_manager.set_cursor_visibility` is permanent for this Wayland connection —
//! a later show does not restore the arrow. `SDL_ShowCursor(false)` is also the wrong hide:
//! the SDL-webOS fork sends hotspot `(0, 0)`, which LSM ignores, so the system arrow stays.
//!
//! LSM treats two reserved `wl_pointer.set_cursor` hotspots as commands, not coordinates
//! (`WebOSCoreCompositor::getCursor` in luna-surfacemanager):
//! - `(254, 254)` → `Qt::BlankCursor` (TV pointer gone)
//! - `(255, 255)` → `Qt::ArrowCursor` (system arrow back)
//!
//! Capture-on sets 254; Capture-off / menu / panic restore sets 255. SDL must keep the
//! cursor *shown* so Wayland actually emits those hotspots instead of the hidden-cursor
//! path. `EVIOCGRAB` still starves mouse reports (no fighting the host); it does not hide
//! the arrow on its own because the Magic Remote remains a pointer source.

use std::ptr::NonNull;
use std::sync::OnceLock;

use sdl2::mouse::MouseUtil;
use sdl2::pixels::{Color, PixelFormatEnum};
use sdl2::surface::Surface;

/// LSM blank-cursor sentinel (`WebOSCoreCompositor::getCursor`).
const HOTSPOT_BLANK: i32 = 254;
/// LSM default-arrow sentinel.
const HOTSPOT_ARROW: i32 = 255;
/// `SDL_CreateColorCursor` rejects a hotspot outside the surface; 255 needs a 256-wide image.
const SENTINEL_SIZE: u32 = 256;

struct LsmSentinels {
    blank: NonNull<sdl2::sys::SDL_Cursor>,
    arrow: NonNull<sdl2::sys::SDL_Cursor>,
}

// Only touched on the SDL video thread; `OnceLock` needs `Sync`.
unsafe impl Send for LsmSentinels {}
unsafe impl Sync for LsmSentinels {}

fn lsm_sentinels() -> Option<&'static LsmSentinels> {
    static SENTINELS: OnceLock<Option<LsmSentinels>> = OnceLock::new();
    SENTINELS.get_or_init(try_create_lsm_sentinels).as_ref()
}

fn try_create_lsm_sentinels() -> Option<LsmSentinels> {
    let blank = make_lsm_cursor(HOTSPOT_BLANK)?;
    let arrow = make_lsm_cursor(HOTSPOT_ARROW)?;
    tracing::info!("LSM cursor sentinels ready (blank {HOTSPOT_BLANK}, arrow {HOTSPOT_ARROW})");
    Some(LsmSentinels { blank, arrow })
}

fn make_lsm_cursor(hotspot: i32) -> Option<NonNull<sdl2::sys::SDL_Cursor>> {
    let mut surface = match Surface::new(SENTINEL_SIZE, SENTINEL_SIZE, PixelFormatEnum::ARGB8888)
        .or_else(|_| Surface::new(SENTINEL_SIZE, SENTINEL_SIZE, PixelFormatEnum::RGBA32))
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("LSM sentinel surface: {e}");
            return None;
        }
    };
    let _ = surface.fill_rect(None, Color::RGBA(0, 0, 0, 0));
    // SAFETY: `surface.raw()` is a live `SDL_Surface` for this call; SDL copies the pixels
    // into the Wayland cursor. The surface may drop afterwards. The `SDL_Cursor` is leaked
    // on purpose — freeing it would `SDL_SetCursor(def)` and undo hotspot 255 on stream exit.
    let raw = match NonNull::new(unsafe { sdl2::sys::SDL_CreateColorCursor(surface.raw(), hotspot, hotspot) }) {
        Some(p) => p,
        None => {
            tracing::warn!(
                "SDL_CreateColorCursor hotspot {hotspot} failed: {}",
                sdl2::get_error()
            );
            return None;
        }
    };
    Some(raw)
}

fn set_lsm_cursor(ptr: NonNull<sdl2::sys::SDL_Cursor>) {
    // SAFETY: `ptr` came from `SDL_CreateColorCursor` and is never freed. SDL video thread.
    unsafe { sdl2::sys::SDL_SetCursor(ptr.as_ptr()) };
}

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

    /// Capture the pointer for the host — LSM blank hotspot, and SDL switched to relative
    /// mode so motion arrives as unbounded deltas instead of coordinates that stop at the
    /// panel edge. Uncaptured is the menu/desktop state: LSM arrow hotspot, absolute.
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
        self.apply_lsm_hotspot();
        self.mouse
            .set_relative_mouse_mode(self.captured && self.sdl_relative);
    }

    fn apply_lsm_hotspot(&mut self) {
        // Keep SDL's cursor *shown*: the fork's hide path sends hotspot (0, 0), which LSM
        // does not treat as blank, so the system arrow stays.
        self.mouse.show_cursor(true);
        let Some(s) = lsm_sentinels() else {
            self.mouse.show_cursor(!self.captured);
            return;
        };
        set_lsm_cursor(if self.captured { s.blank } else { s.arrow });
    }

    /// LSM blank again once the evdev grab has actually landed. No-op while uncaptured.
    pub fn reassert_hidden(&mut self) {
        if !self.captured {
            return;
        }
        self.apply_lsm_hotspot();
    }

    /// webOS can redraw the system arrow on pointer activity; re-send the blank hotspot.
    pub fn on_pointer_activity(&mut self) {
        if self.captured {
            self.apply_lsm_hotspot();
        }
    }
}

/// Panic-hook: LSM arrow hotspot, so a crash mid-capture does not leave the TV without a pointer.
pub fn restore_on_exit() {
    if let Some(s) = lsm_sentinels() {
        set_lsm_cursor(s.arrow);
    }
}
