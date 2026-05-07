#[cfg(debug_assertions)]
use std::path::PathBuf;
use std::sync::Arc;

mod app_root;

use app_root::AppRoot;
#[cfg(target_os = "macos")]
use gpui::{point, TitlebarOptions};
use gpui::{px, size, App, AppContext, Bounds, Menu, MenuItem, WindowBounds, WindowOptions};
use workspace::workspace::{OpenSettings, Quit};
use workspace::{apply_theme, AppSettings, HostBook, Workspace};
#[cfg(debug_assertions)]
use workspace::{Host, HostAuth};

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
  #[cfg_attr(not(debug_assertions), allow(unused_mut))]
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
  #[cfg(debug_assertions)]
  if host_book.is_empty() {
    host_book.add(default_docker_host());
    if let Err(e) = host_book.save() {
      eprintln!("warning: could not seed default host book: {e:#}");
    }
  }

  let settings = AppSettings::load_from(AppSettings::default_path()).unwrap_or_else(|e| {
    eprintln!("warning: failed to load app settings: {e:#}, using defaults");
    AppSettings::default()
  });

  gpui_platform::application()
    .with_assets(ui::AppAssets)
    .run(move |cx: &mut App| {
      gpui_component::init(cx);
      apply_theme(&settings, cx);
      cx.set_global(settings);

      workspace::install_default_keybindings(cx);
      cx.set_menus(build_app_menus());

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
        #[cfg(target_os = "macos")]
        titlebar: Some(macos_titlebar_options()),
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

#[cfg(target_os = "macos")]
fn macos_titlebar_options() -> TitlebarOptions {
  let mut options = gpui_component::TitleBar::title_bar_options();
  options.title = Some("Elum".into());
  // Center traffic lights vertically inside our 34px-high custom title bar.
  options.traffic_light_position = Some(point(px(9.0), px(9.0)));
  options
}

fn build_app_menus() -> Vec<Menu> {
  vec![Menu {
    name: "Elum".into(),
    disabled: false,
    items: vec![
      MenuItem::action("Settings…", OpenSettings),
      MenuItem::separator(),
      MenuItem::action("Quit Elum", Quit),
    ],
  }]
}

#[cfg(debug_assertions)]
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
    auth: HostAuth::PublicKey {
      key_path,
      passphrase_in_keychain: false,
    },
    proxy_jump: None,
  }
}
