//! Rendering markdown: [`parse`] makes a tree of blocks of the text, [`layout`]
//! places them at a width, and [`view`] draws what's on screen and handles
//! clicks. The source stays the only document: everything that changes it,
//! like ticking a task, is a splice of its bytes.

pub mod assets;
pub mod block_edit;
pub mod diagram;
pub mod drawing;
pub mod format;
pub mod images;
pub mod layout;
pub mod links;
pub mod math;
pub mod parse;
pub mod relink;
pub mod table;
pub mod view;
pub mod wysiwyg;

use serde::{Deserialize, Serialize};

pub use self::{
    images::Images,
    links::{Target, is_markdown},
    view::{BlockEditor, Caret, Click, MdView, Options, PageCommand},
};

/// What to do about pictures in documents that are on other servers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RemoteImages {
    /// Show what they are, and a button to load a document's.
    #[default]
    Ask,
    Always,
    Never,
}

/// How a markdown document is shown in its tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Source,
    Split,
    Preview,
    /// The page itself is what's edited. How documents open.
    #[default]
    Wysiwyg,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::Split => "Split",
            Self::Preview => "Preview",
            Self::Wysiwyg => "WYSIWYG",
        }
    }

    /// What's shown in a pane this wide: a phone has no room for two.
    pub fn at_width(self, width: f32) -> Self {
        if self == Self::Split && width < MIN_SPLIT_WIDTH {
            Self::Preview
        } else {
            self
        }
    }
}

/// Narrower than this, Split isn't offered.
pub const MIN_SPLIT_WIDTH: f32 = 560.0;

/// The settings of rendered documents.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub remote_images: RemoteImages,
    /// The mode "Edit" opens a document in.
    pub edit_mode: Mode,
    /// Where pictures put into a document are kept: a folder in its own
    /// folder, or the folder itself when empty.
    pub image_folder: String,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            remote_images: RemoteImages::Ask,
            edit_mode: Mode::Split,
            image_folder: String::new(),
        }
    }
}
