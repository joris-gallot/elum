//! Single source of truth for app keybindings.
//!
//! Actions stay defined in the crate that handles them,
//! but the keystroke -> action mapping lives in one place.
//!
//! Call [`install`] once at app startup. The binary's `main.rs` is the
//! sole caller; nothing else should need to invoke it.

use gpui::{App, KeyBinding};

use crate::workspace::{self, KEY_CONTEXT as WORKSPACE_CONTEXT, PAGE_KEY_CONTEXT as PAGE_CONTEXT};
use terminal::view::{
  Copy as TerminalCopy, Paste as TerminalPaste, SelectAll as TerminalSelectAll,
  KEY_CONTEXT as TERMINAL_CONTEXT,
};

pub fn install(cx: &mut App) {
  cx.bind_keys([
    // Terminal-scoped: only fires when a `TerminalView` is in the focus chain
    KeyBinding::new("cmd-c", TerminalCopy, Some(TERMINAL_CONTEXT)),
    KeyBinding::new("cmd-v", TerminalPaste, Some(TERMINAL_CONTEXT)),
    KeyBinding::new("cmd-a", TerminalSelectAll, Some(TERMINAL_CONTEXT)),
    // Workspace-scoped: fires from anywhere in the workspace
    KeyBinding::new("cmd-w", workspace::CloseTab, Some(WORKSPACE_CONTEXT)),
    KeyBinding::new("cmd-shift-]", workspace::NextTab, Some(WORKSPACE_CONTEXT)),
    KeyBinding::new("cmd-shift-[", workspace::PrevTab, Some(WORKSPACE_CONTEXT)),
    // Global: `None` scope means "fires regardless of focus context".
    KeyBinding::new("cmd-q", workspace::Quit, None),
    KeyBinding::new("cmd-,", workspace::OpenSettings, Some(WORKSPACE_CONTEXT)),
    // Escape on any full-screen page returns the user to the tabs view
    KeyBinding::new("escape", workspace::ClosePage, Some(PAGE_CONTEXT)),
  ]);
}
