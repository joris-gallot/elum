use std::path::PathBuf;
use std::sync::Arc;

use elum_ssh::{ConnectConfig, Session};
use elum_terminal::{view::TerminalView, GridSize, Terminal};
use gpui::{point, px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};

// Initial window size, sized to comfortably fit ~80×24 cells of Menlo 13px.
const INITIAL_COLS: u16 = 80;
const INITIAL_ROWS: u16 = 24;
const INITIAL_WIDTH_PX: f32 = 660.0;
const INITIAL_HEIGHT_PX: f32 = 460.0;

fn main() {
  // Multi-thread tokio runtime that drives russh. Lives until `main`
  // returns, i.e. until the GPUI app exits.
  let runtime = tokio::runtime::Builder::new_multi_thread()
    .enable_all()
    .build()
    .expect("build tokio runtime");

  // Hardcoded for V0: connect to the local docker test sshd.
  let key_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("..")
    .join("..")
    .join("docker")
    .join("sshd")
    .join("fixtures")
    .join("id_ed25519");
  let cfg = ConnectConfig::new("127.0.0.1", 2222, "testuser", &key_path);

  let shell = runtime
    .block_on(async {
      let session = Session::connect(&cfg).await?;
      session.open_shell(INITIAL_COLS, INITIAL_ROWS).await
    })
    .expect("connect + open shell - is `docker compose -f docker/sshd/compose.yml up -d` running?");

  let terminal = Arc::new(Terminal::new(GridSize::new(INITIAL_ROWS, INITIAL_COLS)));
  let from_remote = shell.from_remote.clone();
  let to_remote = shell.to_remote.clone();
  let resize_remote = shell.resize.clone();

  gpui_platform::application().run(move |cx: &mut App| {
    elum_terminal::view::register_default_keybindings(cx);

    let opts = WindowOptions {
      window_bounds: Some(WindowBounds::Windowed(Bounds::new(
        point(px(0.), px(0.)),
        size(px(INITIAL_WIDTH_PX), px(INITIAL_HEIGHT_PX)),
      ))),
      ..Default::default()
    };
    cx.open_window(opts, move |_, cx| {
      cx.new(move |cx| {
        TerminalView::new(terminal, from_remote, to_remote, resize_remote, shell, cx)
      })
    })
    .unwrap();

    cx.activate(true);
  });

  drop(runtime);
}
