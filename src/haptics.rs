//! Haptic feedback: a short buzz as a touch gesture takes effect.
//!
//! Android plays it through the window's view, as its own widgets do, so it
//! follows the phone's "touch feedback" setting and needs no permission.
//! Elsewhere it does nothing: Linux has no API for it, and Windows' (for
//! haptic touchpads, mice and pens: `InputHapticsManager`, SDK 10.0.28000)
//! isn't in the `windows` crate yet.

/// What the buzz marks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Haptic {
    /// A press held long enough: a menu, selecting, or a drag.
    LongPress,
    /// One more (or one fewer) item in a drag selection.
    Tick,
    /// A pinch reached the smallest or largest size.
    Limit,
    /// Something dragged was dropped on a place that took it.
    Confirm,
}

/// Plays `haptic`, if the device can.
pub fn play(haptic: Haptic) {
    #[cfg(target_os = "android")]
    crate::android::haptic(haptic);
    #[cfg(not(target_os = "android"))]
    let _ = haptic;
}
