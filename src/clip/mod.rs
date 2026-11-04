//! Trimming videos and converting them to other formats.
//!
//! [`tab`] is the editor; [`ffmpeg`] does the work, with the user's own ffmpeg
//! in another process, so the app ships no codecs of its own.

mod crop;
mod ffmpeg;
mod tab;

use std::path::Path;

pub use ffmpeg::{ClipJob, Settings, export};
pub use tab::ClipTab;

/// Where the editor can show the video: where the player runs.
const AVAILABLE: bool = cfg!(any(target_os = "linux", windows));

/// Whether the editor can open `path`: ffmpeg needs a file it can reach, which
/// a phone's are, through their mount.
pub fn can_edit(path: &Path) -> bool {
    AVAILABLE
        && crate::video::is_video(path)
        && (!crate::remote::is_remote(path) || crate::phones::is_phone(path))
}
