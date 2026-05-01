mod app;
mod host_book;

use std::path::PathBuf;
use std::sync::Arc;

use app::ElumApp;
use gpui::{point, px, size, App, AppContext, Bounds, WindowBounds, WindowOptions};
use host_book::{Host, HostBook};

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

  // Bootstrap the host book. On first launch, seed it with the docker
  // test container so `cargo run` shows something usable; otherwise load
  // whatever the user has saved.
  let host_book_path = HostBook::default_path();
  let mut host_book = match HostBook::load_from(&host_book_path) {
    Ok(b) => b,
    Err(e) => {
      eprintln!(
        "warning: failed to load host book from {}: {e:#}\nstarting with an empty book",
        host_book_path.display()
      );
      HostBook::load_from(host_book_path.clone())
        .unwrap_or_else(|_| HostBook::load_from(&host_book_path).unwrap())
    }
  };
  if host_book.is_empty() {
    host_book.add(default_docker_host());
    if let Err(e) = host_book.save() {
      eprintln!("warning: could not seed default host book: {e:#}");
    }
  }

  gpui_platform::application().run(move |cx: &mut App| {
    elum_terminal::view::register_default_keybindings(cx);
    app::register_default_keybindings(cx);

    let opts = WindowOptions {
      window_bounds: Some(WindowBounds::Windowed(Bounds::new(
        point(px(0.), px(0.)),
        size(px(INITIAL_WIDTH_PX), px(INITIAL_HEIGHT_PX)),
      ))),
      ..Default::default()
    };
    cx.open_window(opts, move |_, cx| {
      cx.new(move |cx| ElumApp::new(host_book, runtime, cx))
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
  }
}
