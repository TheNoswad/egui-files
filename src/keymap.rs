//! Keyboard shortcuts the user can change. Each command has any number of
//! shortcuts; only the ones changed from the defaults are saved, so changes
//! to the defaults reach everyone else.
//!
//! The keymap lives in the egui context, so menus and tooltips anywhere can
//! show the current shortcut; [`get`] reads it and [`set`] replaces it.
//!
//! Shortcuts match their modifiers exactly, unlike egui's `consume_key`, so
//! Ctrl+Shift+E never also counts as Ctrl+E, whatever the user binds.

use std::{collections::BTreeMap, sync::Arc};

use egui::{Context, Event, InputState, Key, KeyboardShortcut, Modifiers};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Command {
    NewTab,
    CloseTab,
    DiskUsage,
    Settings,
    TogglePreview,
    ToggleTerminal,
    Back,
    Forward,
    Up,
    Refresh,
    EditPath,
    Filter,
    Properties,
    Edit,
    Rename,
    Trash,
    NewFile,
    NewFolder,
    SelectAll,
    ToggleHidden,
    Extract,
    ExtractTo,
}

/// The commands under a heading each, as the settings list them.
pub const GROUPS: [(&str, &[Command]); 3] = [
    (
        "Tabs and panes",
        &[
            Command::NewTab,
            Command::CloseTab,
            Command::DiskUsage,
            Command::Settings,
            Command::TogglePreview,
            Command::ToggleTerminal,
        ],
    ),
    (
        "Navigation",
        &[
            Command::Back,
            Command::Forward,
            Command::Up,
            Command::Refresh,
            Command::EditPath,
            Command::Filter,
        ],
    ),
    (
        "Files",
        &[
            Command::Properties,
            Command::Edit,
            Command::Rename,
            Command::Trash,
            Command::NewFile,
            Command::NewFolder,
            Command::SelectAll,
            Command::ToggleHidden,
            Command::Extract,
            Command::ExtractTo,
        ],
    ),
];

impl Command {
    pub fn label(self) -> &'static str {
        match self {
            Self::NewTab => "New tab",
            Self::CloseTab => "Close tab",
            Self::DiskUsage => "Disk usage",
            Self::Settings => "Settings",
            Self::TogglePreview => "Preview pane",
            Self::ToggleTerminal => "Terminal pane",
            Self::Back => "Back",
            Self::Forward => "Forward",
            Self::Up => "Up a folder",
            Self::Refresh => "Refresh",
            Self::EditPath => "Edit path",
            Self::Filter => "Filter",
            Self::Properties => "Properties",
            Self::Edit => "Edit in a tab",
            Self::Rename => "Rename",
            Self::Trash => "Move to trash",
            Self::NewFile => "New file",
            Self::NewFolder => "New folder",
            Self::SelectAll => "Select all",
            Self::ToggleHidden => "Show hidden files",
            Self::Extract => "Extract here",
            Self::ExtractTo => "Extract to…",
        }
    }

    /// More about what it does, where the label doesn't say.
    pub fn note(self) -> Option<&'static str> {
        match self {
            Self::Trash => Some("In the trash, deletes for good"),
            Self::Extract => {
                Some("Into a folder named after the archive when it has more than one item")
            }
            _ => None,
        }
    }

    pub fn defaults(self) -> Vec<KeyboardShortcut> {
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        let none = Modifiers::NONE;
        let keys: &[(Modifiers, Key)] = match self {
            Self::NewTab => &[(cmd, Key::T)],
            Self::CloseTab => &[(cmd, Key::W)],
            Self::DiskUsage => &[(cmd_shift, Key::U)],
            Self::Settings => &[(cmd, Key::Comma)],
            Self::TogglePreview => &[(none, Key::F3)],
            Self::ToggleTerminal => &[(none, Key::F4)],
            Self::Back => &[(Modifiers::ALT, Key::ArrowLeft)],
            Self::Forward => &[(Modifiers::ALT, Key::ArrowRight)],
            Self::Up => &[(Modifiers::ALT, Key::ArrowUp), (none, Key::Backspace)],
            Self::Refresh => &[(none, Key::F5)],
            Self::EditPath => &[(cmd, Key::L)],
            Self::Filter => &[(cmd, Key::F)],
            Self::Properties => &[(Modifiers::ALT, Key::Enter)],
            Self::Edit => &[(Modifiers::SHIFT, Key::F4)],
            Self::Rename => &[(none, Key::F2)],
            Self::Trash => &[(none, Key::Delete)],
            Self::NewFile => &[(Modifiers::COMMAND.plus(Modifiers::ALT), Key::N)],
            Self::NewFolder => &[(cmd_shift, Key::N)],
            Self::SelectAll => &[(cmd, Key::A)],
            Self::ToggleHidden => &[(cmd, Key::H)],
            Self::Extract => &[(cmd, Key::E)],
            Self::ExtractTo => &[(cmd_shift, Key::E)],
        };
        keys.iter()
            .map(|&(m, k)| KeyboardShortcut::new(m, k))
            .collect()
    }
}

/// Shortcuts that can't be changed, listed in the settings so the list is
/// complete. Copy, cut and paste reach the app as clipboard events rather
/// than key presses, and the arrows and the like move around lists.
pub const FIXED: [(&str, Modifiers, Key); 9] = [
    ("Copy", Modifiers::COMMAND, Key::C),
    ("Cut", Modifiers::COMMAND, Key::X),
    ("Paste", Modifiers::COMMAND, Key::V),
    (
        "Compress",
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::C,
    ),
    ("Open", Modifiers::NONE, Key::Enter),
    ("Move the cursor", Modifiers::NONE, Key::ArrowDown),
    ("Extend the selection", Modifiers::SHIFT, Key::ArrowDown),
    ("First item", Modifiers::NONE, Key::Home),
    ("Last item", Modifiers::NONE, Key::End),
];

/// Keys that keep their own meaning without modifiers: list movement,
/// confirming and cancelling, and focus.
const RESERVED_BARE: [Key; 9] = [
    Key::ArrowUp,
    Key::ArrowDown,
    Key::ArrowLeft,
    Key::ArrowRight,
    Key::Enter,
    Key::Escape,
    Key::Tab,
    Key::Home,
    Key::End,
];

/// Whether `key` is Shift, Ctrl, Alt or Super themselves, which egui reports
/// as keys of their own when pressed. A shortcut is what's pressed with them.
pub fn is_modifier(key: Key) -> bool {
    matches!(
        key,
        Key::ShiftLeft
            | Key::ShiftRight
            | Key::ControlLeft
            | Key::ControlRight
            | Key::AltLeft
            | Key::AltRight
            | Key::SuperLeft
            | Key::SuperRight
    )
}

/// Why a shortcut can't be bound, if it can't.
pub fn refusal(shortcut: KeyboardShortcut) -> Option<String> {
    let KeyboardShortcut {
        modifiers,
        logical_key,
    } = shortcut;
    if let Some((label, ..)) = FIXED
        .iter()
        .find(|(_, m, k)| *k == logical_key && modifiers.matches_exact(*m))
    {
        return Some(format!("That's {label}, which can't be changed"));
    }
    let bare = modifiers.is_none() || modifiers == Modifiers::SHIFT;
    if bare && RESERVED_BARE.contains(&logical_key) {
        return Some("That key moves around lists and dialogs; add a modifier".to_owned());
    }
    None
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Keymap {
    /// The commands whose shortcuts differ from their defaults.
    changed: BTreeMap<Command, Vec<KeyboardShortcut>>,
    /// The settings are waiting for a shortcut to bind; nothing fires.
    #[serde(skip)]
    capturing: bool,
}

impl Keymap {
    pub fn shortcuts(&self, command: Command) -> Vec<KeyboardShortcut> {
        self.changed
            .get(&command)
            .cloned()
            .unwrap_or_else(|| command.defaults())
    }

    pub fn is_default(&self, command: Command) -> bool {
        !self.changed.contains_key(&command)
    }

    fn put(&mut self, command: Command, shortcuts: Vec<KeyboardShortcut>) {
        if shortcuts == command.defaults() {
            self.changed.remove(&command);
        } else {
            self.changed.insert(command, shortcuts);
        }
    }

    /// The command `shortcut` runs, if any.
    pub fn command_for(&self, shortcut: KeyboardShortcut) -> Option<Command> {
        GROUPS
            .iter()
            .flat_map(|(_, commands)| commands.iter().copied())
            .find(|&c| self.shortcuts(c).iter().any(|s| same(*s, shortcut)))
    }

    /// Adds `shortcut` to `command`, taking it from the command that had it,
    /// which is returned.
    pub fn bind(&mut self, command: Command, shortcut: KeyboardShortcut) -> Option<Command> {
        let previous = self.command_for(shortcut);
        if previous == Some(command) {
            return None;
        }
        if let Some(other) = previous {
            let mut theirs = self.shortcuts(other);
            theirs.retain(|s| !same(*s, shortcut));
            self.put(other, theirs);
        }
        let mut ours = self.shortcuts(command);
        ours.push(shortcut);
        self.put(command, ours);
        previous
    }

    pub fn unbind(&mut self, command: Command, shortcut: KeyboardShortcut) {
        let mut ours = self.shortcuts(command);
        ours.retain(|s| !same(*s, shortcut));
        self.put(command, ours);
    }

    /// Puts back `command`'s default shortcuts, taking them from any other
    /// command that has one now.
    pub fn reset(&mut self, command: Command) {
        for shortcut in command.defaults() {
            if let Some(other) = self.command_for(shortcut).filter(|&c| c != command) {
                self.unbind(other, shortcut);
            }
        }
        self.changed.remove(&command);
    }

    pub fn reset_all(&mut self) {
        self.changed.clear();
    }

    pub fn is_capturing(&self) -> bool {
        self.capturing
    }

    pub fn set_capturing(&mut self, capturing: bool) {
        self.capturing = capturing;
    }

    /// Whether one of `command`'s shortcuts was pressed this frame; takes
    /// the key press so nothing else sees it.
    pub fn pressed(&self, input: &mut InputState, command: Command) -> bool {
        if self.capturing {
            return false;
        }
        let shortcuts = self.shortcuts(command);
        let before = input.events.len();
        input.events.retain(|event| {
            !matches!(event, Event::Key { key, modifiers, pressed: true, .. }
                if shortcuts.iter().any(|s| s.logical_key == *key && modifiers.matches_exact(s.modifiers)))
        });
        input.events.len() != before
    }
}

/// The same keys, with Ctrl and Cmd as the platform reads them.
fn same(a: KeyboardShortcut, b: KeyboardShortcut) -> bool {
    a.logical_key == b.logical_key && a.modifiers.matches_exact(b.modifiers)
}

fn id() -> egui::Id {
    egui::Id::new("keymap")
}

pub fn get(ctx: &Context) -> Arc<Keymap> {
    ctx.data(|d| d.get_temp::<Arc<Keymap>>(id()))
        .unwrap_or_default()
}

pub fn set(ctx: &Context, keymap: Keymap) {
    ctx.data_mut(|d| d.insert_temp(id(), Arc::new(keymap)));
}

fn capture_id() -> egui::Id {
    egui::Id::new("keymap-capture-pass")
}

/// Keeps capturing going for this frame; the settings call it each frame
/// they wait for a shortcut.
pub fn keep_capturing(ctx: &Context) {
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| d.insert_temp(capture_id(), pass));
}

/// Stops capturing if the settings didn't wait last frame, as when their
/// tab was closed or hidden meanwhile, so shortcuts don't stay dead.
pub fn end_stale_capture(ctx: &Context) {
    let keymap = get(ctx);
    if !keymap.capturing {
        return;
    }
    let last = ctx.data(|d| d.get_temp::<u64>(capture_id()));
    if last.is_none_or(|pass| pass + 1 < ctx.cumulative_pass_nr()) {
        let mut keymap = (*keymap).clone();
        keymap.capturing = false;
        set(ctx, keymap);
    }
}

/// Whether one of `command`'s shortcuts was pressed; see [`Keymap::pressed`].
pub fn pressed(ctx: &Context, command: Command) -> bool {
    let keymap = get(ctx);
    ctx.input_mut(|i| keymap.pressed(i, command))
}

/// `command`'s first shortcut, as menus show it, or "" with none.
pub fn text(ctx: &Context, command: Command) -> String {
    get(ctx)
        .shortcuts(command)
        .first()
        .map(|s| ctx.format_shortcut(s))
        .unwrap_or_default()
}

/// "`label` (shortcut)", or just `label` when `command` has no shortcut.
pub fn hint(ctx: &Context, label: &str, command: Command) -> String {
    match text(ctx, command) {
        key if key.is_empty() => label.to_owned(),
        key => format!("{label} ({key})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shortcut(modifiers: Modifiers, key: Key) -> KeyboardShortcut {
        KeyboardShortcut::new(modifiers, key)
    }

    #[test]
    fn binding_takes_the_shortcut_from_its_command() {
        let mut keymap = Keymap::default();
        let ctrl_e = shortcut(Modifiers::COMMAND, Key::E);
        assert_eq!(keymap.command_for(ctrl_e), Some(Command::Extract));
        assert_eq!(keymap.bind(Command::Filter, ctrl_e), Some(Command::Extract));
        assert!(keymap.shortcuts(Command::Extract).is_empty());
        assert_eq!(keymap.shortcuts(Command::Filter).len(), 2);
        assert_eq!(keymap.command_for(ctrl_e), Some(Command::Filter));

        // Resetting Extract takes Ctrl+E back.
        keymap.reset(Command::Extract);
        assert_eq!(keymap.command_for(ctrl_e), Some(Command::Extract));
        assert_eq!(
            keymap.shortcuts(Command::Filter),
            Command::Filter.defaults()
        );
        assert!(keymap.is_default(Command::Filter));
        assert_eq!(keymap, Keymap::default());
    }

    #[test]
    fn only_changes_are_saved() {
        let mut keymap = Keymap::default();
        keymap.unbind(Command::Up, shortcut(Modifiers::NONE, Key::Backspace));
        let saved = ron::to_string(&keymap).unwrap();
        let loaded: Keymap = ron::from_str(&saved).unwrap();
        assert_eq!(loaded, keymap);
        assert_eq!(loaded.changed.len(), 1);
        assert_eq!(
            loaded.shortcuts(Command::Up),
            [shortcut(Modifiers::ALT, Key::ArrowUp)]
        );
    }

    #[test]
    fn modifiers_match_exactly() {
        let keymap = Keymap::default();
        let press = |modifiers| {
            let mut input = InputState::default();
            input.events.push(Event::Key {
                key: Key::E,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            });
            (
                keymap.pressed(&mut input.clone(), Command::Extract),
                keymap.pressed(&mut input, Command::ExtractTo),
            )
        };
        assert_eq!(press(Modifiers::COMMAND), (true, false));
        assert_eq!(
            press(Modifiers::COMMAND.plus(Modifiers::SHIFT)),
            (false, true)
        );
        assert_eq!(press(Modifiers::NONE), (false, false));
    }

    #[test]
    fn capture_ends_when_the_settings_stop_waiting() {
        let ctx = Context::default();
        // Each pass: what the app does first, then the settings page if shown.
        let pass = |settings_shown: bool, start: bool| {
            let mut output = ctx.run_ui(Default::default(), |ui| {
                let ctx = ui.ctx();
                end_stale_capture(ctx);
                if start {
                    let mut keymap = (*get(ctx)).clone();
                    keymap.set_capturing(true);
                    set(ctx, keymap);
                }
                if settings_shown && get(ctx).is_capturing() {
                    keep_capturing(ctx);
                }
            });
            output.textures_delta.clear();
        };
        pass(true, true);
        pass(true, false);
        pass(true, false);
        assert!(get(&ctx).is_capturing());
        // The settings tab is hidden: the next pass finds it didn't wait.
        pass(false, false);
        pass(false, false);
        assert!(!get(&ctx).is_capturing());
    }

    #[test]
    fn modifier_keys_alone_are_not_shortcuts() {
        assert!(is_modifier(Key::ControlLeft));
        assert!(is_modifier(Key::SuperRight));
        assert!(!is_modifier(Key::K));
    }

    #[test]
    fn fixed_and_list_keys_are_refused() {
        assert!(refusal(shortcut(Modifiers::COMMAND, Key::C)).is_some());
        assert!(refusal(shortcut(Modifiers::NONE, Key::ArrowDown)).is_some());
        assert!(refusal(shortcut(Modifiers::ALT, Key::ArrowDown)).is_none());
        assert!(refusal(shortcut(Modifiers::NONE, Key::F6)).is_none());
    }

    #[test]
    fn defaults_dont_clash() {
        let keymap = Keymap::default();
        for (_, commands) in GROUPS {
            for &command in commands {
                for shortcut in command.defaults() {
                    assert_eq!(keymap.command_for(shortcut), Some(command), "{shortcut:?}");
                    assert_eq!(refusal(shortcut), None, "{command:?}");
                }
            }
        }
        let listed: usize = GROUPS.iter().map(|(_, c)| c.len()).sum();
        assert_eq!(listed, 22, "every command is listed once");
    }
}
