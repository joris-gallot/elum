# Elum

A native SSH terminal client built with [GPUI](https://github.com/zed-industries/zed).

## Features

- Multi-tab SSH sessions
- Host book with password / key auth (secrets stored in OS keychain)
- Light + dark themes with auto-switching
- Configurable terminal themes

## Build

```sh
cargo run
```

## Crates

- `elum` - app entry point
- `workspace` - main window, sidebar, tabs, settings
- `terminal` - terminal view and rendering
- `ssh` - SSH transport
- `ui` - shared UI bits (dialogs, icons)
