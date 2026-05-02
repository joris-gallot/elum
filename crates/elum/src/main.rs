use std::path::PathBuf;
use std::sync::Arc;

mod app_root;

use app_root::AppRoot;
use elum_workspace::{Host, HostBook, Workspace};
use gpui::{px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};

const INITIAL_WIDTH_PX: f32 = 900.0;
const INITIAL_HEIGHT_PX: f32 = 540.0;

fn main() {
  // Multi-thread tokio runtime that drives russh. Lives until `main`
  // returns, i.e. until the GPUI app exits.
  let runtime = Arc::new(
    tokio::runtime::Builder::new_multi_thread()
      .enable_all()
      .build()
      .expect("build tokio runtime"),
  );

  let host_book_path = HostBook::default_path();
  let mut host_book = match HostBook::load_from(&host_book_path) {
    Ok(b) => b,
    Err(e) => {
      eprintln!(
        "warning: failed to load host book from {}: {e:#}\nstarting with an empty book",
        host_book_path.display()
      );
      HostBook::load_from(&host_book_path)
        .unwrap_or_else(|_| HostBook::load_from(host_book_path.clone()).expect("retry load empty"))
    }
  };
  if host_book.is_empty() {
    // Seed with the default Docker test if the book is empty
    host_book.add(default_docker_host());
    if let Err(e) = host_book.save() {
      eprintln!("warning: could not seed default host book: {e:#}");
    }
  }

  gpui_platform::application()
    .with_assets(elum_ui::AppAssets)
    .run(move |cx: &mut App| {
      gpui_component::init(cx);
      gpui_component::Theme::change(gpui_component::ThemeMode::Dark, None, cx);

      elum_workspace::install_default_keybindings(cx);

      // Closing the last window quits the app
      cx.on_window_closed(|cx, _| {
        if cx.windows().is_empty() {
          cx.quit();
        }
      })
      .detach();

      let bounds = Bounds::centered(None, size(px(INITIAL_WIDTH_PX), px(INITIAL_HEIGHT_PX)), cx);
      let opts = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        ..Default::default()
      };
      cx.open_window(opts, move |window, cx| {
        let workspace = cx.new(|cx| Workspace::new(host_book, runtime, window, cx));
        let app_root = cx.new(|cx| AppRoot::new(workspace, window, cx));
        cx.new(|cx| gpui_component::Root::new(app_root, window, cx))
      })
      .unwrap();

      cx.activate(true);
    });
}

/// First-launch seed: a host pointing at the local SSH test container so
/// `cargo run` works out of the box once `docker compose up` is running.
fn default_docker_host() -> Host {
  let key_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("..")
    .join("..")
    .join("docker")
    .join("sshd")
    .join("fixtures")
    .join("id_ed25519");
  Host {
    id: "docker-test".into(),
    name: "Docker Test".into(),
    host: "127.0.0.1".into(),
    port: 2222,
    user: "testuser".into(),
    key_path,
    passphrase_in_keychain: false,
  }
}
