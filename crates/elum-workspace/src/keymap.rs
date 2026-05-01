//! Single source of truth for app keybindings.
//!
//! Actions stay defined in the crate that handles them (e.g. `Copy` /
//! `Paste` / `SelectAll` in `elum-terminal`, `CloseTab` / `Quit` here),
//! but the keystroke -> action mapping lives in one place.
//!
//! Call [`install`] once at app startup. The binary's `main.rs` is the
//! sole caller; nothing else should need to invoke it.

use gpui::{App, KeyBinding};

use crate::workspace::{self, KEY_CONTEXT as WORKSPACE_CONTEXT};
use elum_terminal::view::{
  Copy as TerminalCopy, Paste as TerminalPaste, SelectAll as TerminalSelectAll,
  KEY_CONTEXT as TERMINAL_CONTEXT,
};

pub fn install(cx: &mut App) {
  cx.bind_keys([
    // Terminal-scoped: only fires when a `TerminalView` is in the focus
    // chain. Lets us keep app-level Cmd-C/V/A reserved for chrome later.
    KeyBinding::new("cmd-c", TerminalCopy, Some(TERMINAL_CONTEXT)),
    KeyBinding::new("cmd-v", TerminalPaste, Some(TERMINAL_CONTEXT)),
    KeyBinding::new("cmd-a", TerminalSelectAll, Some(TERMINAL_CONTEXT)),
    // Workspace-scoped: fires from anywhere in the workspace, including
    // a focused terminal (action dispatch bubbles outward).
    KeyBinding::new("cmd-w", workspace::CloseTab, Some(WORKSPACE_CONTEXT)),
    KeyBinding::new("cmd-shift-]", workspace::NextTab, Some(WORKSPACE_CONTEXT)),
    KeyBinding::new("cmd-shift-[", workspace::PrevTab, Some(WORKSPACE_CONTEXT)),
    // Global: `None` scope means "fires regardless of focus context".
    // Cmd-Q is the macOS standard quit shortcut.
    KeyBinding::new("cmd-q", workspace::Quit, None),
  ]);
}
