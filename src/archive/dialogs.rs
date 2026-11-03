//! The Compress and Extract-to dialogs.

use std::path::{Path, PathBuf};

use egui::{Button, ComboBox, Context, Id, Key, Modal, RichText, Slider, TextEdit, Ui, vec2};
use egui_phosphor::regular as icon;

use super::{ExtractJob, ExtractMode, Format, Prefs, Selection, stem};
use crate::{
    fs::{display_name, unique_path},
    jobs::JobKind,
    places::{self, Place},
    tree::{FolderTree, TreeAction},
};

pub enum DialogResult {
    Open,
    Closed,
    Start(Vec<JobKind>),
}

/// A folder tree to pick a destination in, with an editable path.
struct FolderPicker {
    tree: FolderTree,
    roots: Vec<Place>,
    current: PathBuf,
    text: String,
    error: Option<String>,
}

impl FolderPicker {
    fn new(current: PathBuf) -> Self {
        let mut roots: Vec<Place> = places::standard_places().into_iter().take(1).collect();
        roots.extend(places::drives());
        Self {
            tree: FolderTree::picker(),
            roots,
            text: current.display().to_string(),
            current,
            error: None,
        }
    }

    fn set(&mut self, path: PathBuf) {
        self.text = path.display().to_string();
        self.current = path;
    }

    fn ui(&mut self, ui: &mut Ui, show_hidden: bool) {
        ui.horizontal(|ui| {
            // Room for the new folder button, in a dialog as narrow as a phone.
            let width = (ui.available_width() - 48.0).clamp(80.0, 340.0);
            let edit = ui.add(TextEdit::singleline(&mut self.text).desired_width(width));
            if edit.lost_focus() {
                let typed = PathBuf::from(self.text.trim());
                if disk::is_dir(&typed) {
                    self.current = typed;
                } else {
                    self.text = self.current.display().to_string();
                }
            }
            if ui
                .button(icon::FOLDER_PLUS)
                .on_hover_text("New folder")
                .clicked()
            {
                match crate::fs::create_dir(&self.current) {
                    Ok(path) => {
                        self.tree.dirs_changed(&[self.current.clone()].into());
                        self.set(path);
                    }
                    Err(err) => self.error = Some(format!("Could not create folder: {err}")),
                }
            }
        });
        egui::Frame::group(ui.style()).show(ui, |ui| {
            let size = vec2(ui.available_width().min(400.0), 240.0);
            ui.set_min_size(size);
            ui.set_max_size(size);
            let roots: Vec<&Place> = self.roots.iter().collect();
            if let Some(TreeAction::Navigate(path) | TreeAction::OpenInNewTab(path)) =
                self.tree.ui(ui, &roots, Some(&self.current), show_hidden)
            {
                self.set(path);
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
    }
}

fn level_name(level: u32, format: Format) -> &'static str {
    match level {
        0 if format == Format::Zip || format == Format::SevenZ => "Store (no compression)",
        0..=2 => "Fastest",
        3..=4 => "Fast",
        5..=6 => "Normal",
        7..=8 => "Maximum",
        _ => "Ultra",
    }
}

pub struct CompressDialog {
    id: Id,
    sources: Vec<PathBuf>,
    name: String,
    format: Format,
    level: u32,
    password: String,
    confirm: String,
    show_password: bool,
    encrypt_names: bool,
    picker: FolderPicker,
    choosing_folder: bool,
    just_opened: bool,
}

impl CompressDialog {
    pub fn new(id: Id, sources: Vec<PathBuf>, prefs: &Prefs) -> Self {
        let dir = sources
            .first()
            .and_then(|p| p.parent())
            .map_or_else(places::home_dir, Path::to_path_buf);
        let name = match sources.as_slice() {
            [single] if disk::is_dir(single) => display_name(single),
            [single] => single.file_stem().map_or_else(
                || display_name(single),
                |s| s.to_string_lossy().into_owned(),
            ),
            _ => match dir.file_name() {
                Some(name) => name.to_string_lossy().into_owned(),
                None => "Archive".to_owned(),
            },
        };
        Self {
            id,
            sources,
            name,
            format: prefs.format,
            level: prefs.level,
            password: String::new(),
            confirm: String::new(),
            show_password: false,
            encrypt_names: prefs.encrypt_names,
            picker: FolderPicker::new(dir),
            choosing_folder: false,
            just_opened: true,
        }
    }

    fn file_name(&self) -> String {
        format!("{}{}", self.name.trim(), self.format.extension())
    }

    pub fn show(&mut self, ctx: &Context, prefs: &mut Prefs, show_hidden: bool) -> DialogResult {
        let mut start = false;
        let mut cancel = false;
        let modal = Modal::new(self.id).show(ctx, |ui| {
            let width = crate::widgets::modal_width(ui, 440.0);
            ui.set_width(width);
            // The text fields leave room for the labels and the buttons
            // beside them, on a phone too.
            let field = (width - 190.0).clamp(80.0, 220.0);
            ui.heading(format!("{} Compress", icon::FILE_ZIP));
            let what = match self.sources.as_slice() {
                [single] => format!("“{}”", display_name(single)),
                many => format!("{} items", many.len()),
            };
            ui.weak(what);
            ui.add_space(6.0);

            let name_invalid =
                self.name.trim().is_empty() || self.name.contains(['/', std::path::MAIN_SEPARATOR]);
            let passwords_differ = self.format.supports_password() && self.password != self.confirm;
            egui::Grid::new(self.id.with("grid"))
                .num_columns(2)
                .spacing(vec2(8.0, 6.0))
                .show(ui, |ui| {
                    ui.label("Name:");
                    ui.horizontal(|ui| {
                        let edit =
                            ui.add(TextEdit::singleline(&mut self.name).desired_width(field));
                        if std::mem::take(&mut self.just_opened) {
                            edit.request_focus();
                        }
                        if edit.lost_focus()
                            && ui.input(|i| i.key_pressed(Key::Enter))
                            && !name_invalid
                            && !passwords_differ
                        {
                            start = true;
                        }
                        ComboBox::from_id_salt(self.id.with("format"))
                            .selected_text(self.format.extension())
                            .show_ui(ui, |ui| {
                                for format in Format::CREATE {
                                    let label =
                                        format!("{}  ({})", format.extension(), format.label());
                                    ui.selectable_value(&mut self.format, format, label);
                                }
                            });
                    });
                    ui.end_row();

                    ui.label("Save in:");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(self.picker.current.display().to_string()).truncate(),
                        );
                    });
                    ui.end_row();
                    ui.label("");
                    let toggle = if self.choosing_folder {
                        format!("{} Hide folders", icon::CARET_UP)
                    } else {
                        format!("{} Choose folder…", icon::FOLDER_OPEN)
                    };
                    if ui.button(toggle).clicked() {
                        self.choosing_folder = !self.choosing_folder;
                    }
                    ui.end_row();

                    ui.label("Compression:");
                    let min = if matches!(self.format, Format::Zip | Format::SevenZ) {
                        0
                    } else {
                        1
                    };
                    self.level = self.level.max(min);
                    let name = level_name(self.level, self.format);
                    ui.add(
                        Slider::new(&mut self.level, min..=9)
                            .show_value(false)
                            .text(name),
                    );
                    ui.end_row();

                    let can_encrypt = self.format.supports_password();
                    let why_not =
                        "Tar archives can't be encrypted. Choose .zip or .7z for a password.";
                    ui.label("Password:");
                    ui.horizontal(|ui| {
                        let edit = TextEdit::singleline(&mut self.password)
                            .password(!self.show_password)
                            .hint_text("None")
                            .desired_width(field);
                        ui.add_enabled(can_encrypt, edit)
                            .on_disabled_hover_text(why_not);
                        let eye = if self.show_password {
                            icon::EYE_SLASH
                        } else {
                            icon::EYE
                        };
                        if ui
                            .add_enabled(can_encrypt, Button::new(eye))
                            .on_hover_text("Show password")
                            .clicked()
                        {
                            self.show_password = !self.show_password;
                        }
                    });
                    ui.end_row();
                    if can_encrypt && !self.password.is_empty() {
                        ui.label("Repeat:");
                        ui.add(
                            TextEdit::singleline(&mut self.confirm)
                                .password(!self.show_password)
                                .desired_width(field),
                        );
                        ui.end_row();
                        ui.label("");
                        let names = ui.add_enabled(
                            self.format == Format::SevenZ,
                            egui::Checkbox::new(&mut self.encrypt_names, "Encrypt file names"),
                        );
                        names.on_disabled_hover_text(
                            "ZIP can't hide file names, only their contents. Use .7z for that.",
                        );
                        ui.end_row();
                    }
                });

            if self.choosing_folder {
                ui.add_space(4.0);
                self.picker.ui(ui, show_hidden);
            }

            ui.add_space(4.0);
            let target = self.picker.current.join(self.file_name());
            if passwords_differ && !self.confirm.is_empty() {
                ui.colored_label(ui.visuals().error_fg_color, "The passwords don't match.");
            } else if !name_invalid && disk::symlink_metadata(&target).is_ok() {
                let free = unique_path(&self.picker.current, self.file_name().as_ref());
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    format!(
                        "“{}” already exists; this will be saved as “{}”.",
                        self.file_name(),
                        display_name(&free)
                    ),
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let ready = !name_invalid && !passwords_differ;
                if ui
                    .add_enabled(ready, Button::new(RichText::new("Compress").strong()))
                    .clicked()
                {
                    start = true;
                }
                cancel = ui.button("Cancel").clicked();
            });
        });
        if start {
            prefs.format = self.format;
            prefs.level = self.level;
            if self.format == Format::SevenZ && !self.password.is_empty() {
                prefs.encrypt_names = self.encrypt_names;
            }
            let password = (self.format.supports_password() && !self.password.is_empty())
                .then(|| self.password.clone());
            return DialogResult::Start(vec![JobKind::Compress(super::CompressJob {
                sources: self.sources.clone(),
                dest: self.picker.current.join(self.file_name()),
                format: self.format,
                level: self.level,
                password,
                encrypt_names: self.format == Format::SevenZ && self.encrypt_names,
            })]);
        }
        if cancel || modal.should_close() {
            return DialogResult::Closed;
        }
        DialogResult::Open
    }
}

pub struct ExtractDialog {
    id: Id,
    archives: Vec<PathBuf>,
    selection: Option<Selection>,
    password: Option<String>,
    mode: ExtractMode,
    picker: FolderPicker,
}

impl ExtractDialog {
    pub fn new(
        id: Id,
        archives: Vec<PathBuf>,
        selection: Option<Selection>,
        password: Option<String>,
    ) -> Self {
        let dir = archives
            .first()
            .and_then(|p| p.parent())
            .map_or_else(places::home_dir, Path::to_path_buf);
        Self {
            id,
            archives,
            mode: if selection.is_some() {
                ExtractMode::Into
            } else {
                ExtractMode::Smart
            },
            selection,
            password,
            picker: FolderPicker::new(dir),
        }
    }

    pub fn show(&mut self, ctx: &Context, prefs: &mut Prefs, show_hidden: bool) -> DialogResult {
        let mut start = false;
        let mut cancel = false;
        let modal = Modal::new(self.id).show(ctx, |ui| {
            ui.set_width(crate::widgets::modal_width(ui, 420.0));
            ui.heading(format!("{} Extract to…", icon::ARCHIVE));
            let what = match (&self.selection, self.archives.as_slice()) {
                (Some(selection), [archive]) => format!(
                    "{} item{} from “{}”",
                    selection.paths.len(),
                    if selection.paths.len() == 1 { "" } else { "s" },
                    display_name(archive)
                ),
                (_, [archive]) => format!("“{}”", display_name(archive)),
                (_, many) => format!("{} archives", many.len()),
            };
            ui.weak(what);
            ui.add_space(6.0);
            self.picker.ui(ui, show_hidden);
            ui.add_space(6.0);
            let folder_name = match self.archives.as_slice() {
                [archive] => format!("In a folder named “{}”", stem(archive)),
                _ => "Each in a folder named after the archive".to_owned(),
            };
            ui.radio_value(
                &mut self.mode,
                ExtractMode::Smart,
                "Automatically: one item goes straight in, several get a folder",
            );
            ui.radio_value(&mut self.mode, ExtractMode::Subfolder, folder_name);
            ui.radio_value(
                &mut self.mode,
                ExtractMode::Into,
                "Directly into this folder",
            );
            if self.selection.is_none() {
                ui.checkbox(
                    &mut prefs.trash_after_extract,
                    "Move the archive to the trash afterwards",
                );
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add(Button::new(RichText::new("Extract").strong()))
                    .clicked()
                {
                    start = true;
                }
                cancel = ui.button("Cancel").clicked();
            });
        });
        if start {
            let jobs = self
                .archives
                .iter()
                .map(|archive| {
                    let mut job = ExtractJob::new(
                        archive.clone(),
                        self.picker.current.clone(),
                        self.mode,
                        self.selection.is_none() && prefs.trash_after_extract,
                    );
                    job.selection = self.selection.clone();
                    job.password = self.password.clone();
                    JobKind::Extract(job)
                })
                .collect();
            return DialogResult::Start(jobs);
        }
        if cancel || modal.should_close() {
            return DialogResult::Closed;
        }
        DialogResult::Open
    }
}

/// Folder names for the context menu: `Extract to “name/”`.
pub fn subfolder_label(archives: &[PathBuf]) -> String {
    match archives {
        [archive] => format!("Extract to “{}/”", stem(archive)),
        _ => "Extract each to its own folder".to_owned(),
    }
}
