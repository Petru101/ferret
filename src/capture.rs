// Capturing the game's window: only that window, never the rest of the desktop. Linux: the
// ScreenCast portal (the player picks the window); Windows: Windows.Graphics.Capture on the
// attached game's main window.

#[cfg(not(windows))]
#[path = "capture_linux.rs"]
mod imp;
#[cfg(windows)]
#[path = "capture_windows.rs"]
mod imp;

pub use imp::WindowCapture;
