//! Experimental macOS client base: GTK UI, SSH and native VideoToolbox video decoding.
//! Native-layout keyboard input; integration against Linux still needs manual QA.

mod app_icon;
mod clipboard;
mod clipboard_text;
mod clipboard_ui;
mod geometry;
mod input_queue;
mod keyboard;
mod keymap;
mod macos_keyboard;
mod net;
mod paintable;
mod recent;
mod system_capture;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{channel, sync_channel, Receiver};
use std::time::{Duration, Instant};

use adw::prelude::*;
use clap::Parser;
use gliff_proto::{Axis, ClientMsg};
use gliff_transport::SshTarget;
use gtk::gdk;
use gtk::glib;
use gtk4 as gtk;
use libadwaita as adw;
use net::Frame;
use net::{Endpoint, Picture, Status, Worker};
use recent::Config;

/// A message plus optional trailing payload, sent from the UI to the worker.
type OutSender = input_queue::Sender;

const APP_NAME: &str = "gliff";

#[derive(Parser, Clone)]
#[command(
    name = APP_NAME,
    version,
    about = "Remote-desktop a Hyprland session over ssh"
)]
struct Cli {
    /// `user@host` to ssh to and spawn gliff-server. Without it the window
    /// opens with a tab for each machine connected to before, or with the
    /// address bar focused when there are none.
    host: Option<String>,
    /// Disable clipboard sharing in both directions for every tab.
    #[arg(long)]
    no_clipboard: bool,
    /// Dev: connect directly to a `gliff-server --listen` address. Repeat it
    /// to open a tab for each.
    #[arg(long)]
    connect: Vec<String>,
    /// Remote gliff-server path.
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    /// Create a private headless output on the remote, sized and scaled to
    /// this window, instead of mirroring the remote's focused screen.
    #[arg(long, conflicts_with = "output")]
    headless: bool,
    /// Mirror the named remote output (e.g. `DP-1`) instead of the focused one.
    #[arg(long)]
    output: Option<String>,
    /// Release remote keys and leave video focus. System shortcuts stay local
    /// unless --capture-system-shortcuts is enabled. Forms: `shift+escape`, `ctrl+alt+q`
    /// or `super+escape`; `double-<key>` for a double-tap (e.g.
    /// `double-escape`); or `none` to disable. The screen recaptures when you
    /// click it again.
    #[arg(long, default_value = "shift+escape")]
    release_hotkey: String,
    /// Map Command to Linux Super (default) or Control. Does not override macOS shortcuts.
    #[arg(long, value_enum, default_value = "super")]
    command_key: keyboard::Command,
    /// Option as Linux Alt (default), or AltGr for native Option symbols/dead keys.
    #[arg(long, value_enum, default_value = "alt")]
    option_key: keyboard::OptionKey,
    /// Opt-in native session keyboard filter (Accessibility required). Shift+Esc always releases.
    #[arg(long)]
    capture_system_shortcuts: bool,
    /// Explicitly request Accessibility once, print diagnostic and exit. No window/tap/capture.
    #[arg(long, exclusive = true)]
    request_capture_permissions: bool,
}

/// Everything the UI shares with its callbacks.
struct App {
    window: adw::ApplicationWindow,
    /// The picture, upcast; input controllers attach to it and we measure it.
    video: gtk::Widget,
    /// What the picture shows: the latest frame fitted to the window.
    frame: paintable::FramePaintable,
    stats: gtk::Label,
    /// Centred over the picture while the shown session is not streaming:
    /// connecting, reconnecting, an error, or no session at all.
    status: gtk::Label,
    /// A short-lived note near the bottom of the picture, such as how to
    /// release the captured shortcuts.
    hint: gtk::Label,
    /// Bumped by every hint, so a newer one is not hidden by an older timer.
    hint_generation: Cell<u64>,
    /// A tab per machine, to the left of the address bar.
    tabs: gtk::Box,
    /// Expands into the address bar; hidden while the address bar shows.
    add_btn: gtk::Button,
    /// Holds the address bar, which shows only while adding a machine, or
    /// while there are no tabs at all.
    add_revealer: gtk::Revealer,
    /// The address bar: Enter connects to the machine typed.
    entry: gtk::Entry,
    transfers: Rc<clipboard_ui::TransferBars>,
    remote_clipboard: Rc<clipboard_ui::RemoteClipboard>,
    /// Every running connection, in the order they were opened.
    sessions: RefCell<Vec<Rc<Session>>>,
    /// The session the window shows and sends input to.
    active: RefCell<Option<Rc<Session>>>,
    /// Size of the video widget in device pixels, rounded down to even.
    view_size: Cell<(u32, u32)>,
    view_scale: Cell<f64>,
    /// Set by the release hotkey: the pointer is over the picture but the
    /// keyboard stays local until the pointer leaves, or the picture is
    /// clicked.
    released: Cell<bool>,
    config_path: PathBuf,
    cli: Cli,
    keymap: RefCell<keymap::Source>,
    local_keys: RefCell<keyboard::LocalKeys>,
    closing: Cell<bool>,
    native_capture: RefCell<system_capture::Driver>,
    capture_status: gtk::Label,
}

/// One connection to a machine. Every session keeps streaming while the
/// window shows another, so switching tabs is instant; only the shown one
/// gets input.
struct Session {
    /// The tab's name: `user@host`, or the address of a dev `--connect`.
    name: String,
    endpoint: Endpoint,
    input_tx: RefCell<Option<OutSender>>,
    keyboard: keyboard::Shared,
    /// Bumped by every `start_session` and by `stop_session`, so pollers and
    /// delayed reconnects of an earlier worker can tell they are stale.
    generation: Cell<u64>,
    /// Consecutive failed connection attempts, reset on a successful connect.
    retries: Cell<u32>,
    /// Automatic reconnects have given up; selecting the tab tries again.
    gave_up: Cell<bool>,
    /// Recorded in the config on the first successful connect.
    remembered: Cell<bool>,
    /// The server is streaming: set by its Connected status, cleared when
    /// the connection drops.
    connected: Cell<bool>,
    /// The server streams a dedicated headless output: its frames carry the
    /// compositor-drawn cursor, so the local pointer is hidden over the video.
    headless: Cell<bool>,
    /// Size of the stream the server is sending, from the last decoded frame.
    stream_size: Cell<(u32, u32)>,
    /// The full-quality fit size the stream is drawn into; equals the stream
    /// size unless the server reduced the resolution. From the last decoded
    /// frame, so pointer mapping always matches the picture on screen.
    stream_view: Cell<(u32, u32)>,
    /// The view from the last StreamConfig (which may not be painted yet);
    /// only the resize gate reads it.
    server_view: Cell<(u32, u32)>,
    /// The server's frame-rate ceiling, for the stats overlay.
    fps_cap: Cell<u32>,
    /// The remote output's scale: pointer coordinates go in physical / scale.
    stream_scale: Cell<f32>,
    /// The size last asked of the server, so a pending resize is not repeated.
    resize_requested: Cell<(u32, u32)>,
    resize_scale: Cell<f64>,
    /// The output zoom last asked of the decoder; a new worker starts at 1.
    zoom: Cell<u32>,
    /// The latest frame and its view, painted when the tab is shown.
    last_frame: RefCell<Option<(gdk::Texture, (u32, u32))>>,
    cursor: RefCell<Option<gdk::Cursor>>,
    status: RefCell<String>,
    stats: RefCell<String>,
}

impl Session {
    fn reset_resize(&self) {
        self.resize_requested.set((0, 0));
        self.resize_scale.set(0.0);
    }

    fn new(endpoint: Endpoint) -> Self {
        Self {
            name: machine_name(&endpoint).to_string(),
            endpoint,
            input_tx: RefCell::new(None),
            keyboard: keyboard::Shared::default(),
            generation: Cell::new(0),
            retries: Cell::new(0),
            gave_up: Cell::new(false),
            remembered: Cell::new(false),
            connected: Cell::new(false),
            headless: Cell::new(false),
            stream_size: Cell::new((0, 0)),
            stream_view: Cell::new((0, 0)),
            server_view: Cell::new((0, 0)),
            fps_cap: Cell::new(0),
            stream_scale: Cell::new(1.0),
            resize_requested: Cell::new((0, 0)),
            resize_scale: Cell::new(0.0),
            zoom: Cell::new(1),
            last_frame: RefCell::new(None),
            cursor: RefCell::new(None),
            status: RefCell::new(String::new()),
            stats: RefCell::new(String::new()),
        }
    }
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    if let Some(result) = system_capture::explicit_permission_request(
        cli.request_capture_permissions,
        system_capture::request_capture_permission,
    ) {
        println!("Capture permission request: executable={} pid={}; TCC may attribute this to your terminal, not Gliff.", std::env::current_exe().map(|p|p.display().to_string()).unwrap_or_else(|_|"unknown".into()), std::process::id());
        match result {
            Ok(trusted) => println!("AX trusted now={trusted}. Prompt is asynchronous, not a grant. Review Privacy & Security in Settings, then restart with --capture-system-shortcuts and click connected video. No tap/window was started."),
            Err(error) => { eprintln!("Permission request failed: {error}; no tap/window started"); return glib::ExitCode::FAILURE; }
        }
        return glib::ExitCode::SUCCESS;
    }
    // Keep a stable application name for GTK.
    glib::set_prgname(Some(APP_NAME));
    let app = adw::Application::builder().build();
    app.connect_startup(|_| {
        gtk::Window::set_default_icon_name(APP_NAME);
        if let Err(error) = app_icon::install() {
            tracing::warn!(%error, "could not set the native application icon");
        }
        tracing::info!("macOS client: native keyboard enabled; remote integration requires manual verification");
    });
    app.connect_activate(move |app| build_ui(app, &cli));
    // GTK owns argv parsing; we already parsed with clap, so pass none.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty)
}

/// The endpoints named on the command line, if any.
fn endpoints_from_cli(cli: &Cli) -> Vec<Endpoint> {
    if !cli.connect.is_empty() {
        return cli.connect.iter().cloned().map(Endpoint::Tcp).collect();
    }
    cli.host
        .iter()
        .map(|host| ssh_endpoint(cli, host))
        .collect()
}

fn ssh_endpoint(cli: &Cli, host: &str) -> Endpoint {
    let mut t = SshTarget::new(host.to_string());
    t.server_bin = cli.server_bin.clone();
    t.server_args = match (&cli.output, cli.headless) {
        (Some(name), _) => vec!["--output".into(), name.clone()],
        (None, true) => vec!["--headless".into()],
        (None, false) => vec!["--output".into(), "auto".into()],
    };
    Endpoint::Ssh(t)
}

/// The tab name for an endpoint.
fn machine_name(endpoint: &Endpoint) -> &str {
    match endpoint {
        Endpoint::Ssh(t) => t.host.as_str(),
        Endpoint::Tcp(addr) => addr.as_str(),
    }
}

/// Title in the `Document — App` form used by GNOME apps, so the machine
/// is what a task switcher shows first.
fn window_title(endpoint: &Endpoint) -> String {
    format!("{} — Gliff", machine_name(endpoint))
}

/// The one window: a header with a tab per machine and the address bar, the
/// remote screen (black until connected) with any status over it.
fn build_ui(app: &adw::Application, cli: &Cli) {
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Gliff")
        .default_width(1280)
        .default_height(760)
        .build();

    let header = adw::HeaderBar::builder().show_title(false).build();
    let entry = gtk::Entry::builder()
        .placeholder_text("user@host")
        .width_chars(32)
        .build();
    let connect_btn = gtk::Button::builder()
        .icon_name(CONNECT_ICON)
        .tooltip_text("Connect")
        .build();
    let address_bar = gtk::Box::builder().css_classes(["linked"]).build();
    address_bar.append(&entry);
    address_bar.append(&connect_btn);
    let add_revealer = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideRight)
        .child(&address_bar)
        .build();
    let add_btn = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("Connect to another machine")
        .css_classes(["flat"])
        .build();
    let fullscreen_btn = gtk::ToggleButton::builder()
        .icon_name("view-fullscreen-symbolic")
        .build();
    let stats_btn = gtk::ToggleButton::builder()
        .icon_name("utilities-system-monitor-symbolic")
        .tooltip_text("Show stats")
        .build();
    let tabs = gtk::Box::builder().spacing(6).build();
    let tabs_scroller = gtk::ScrolledWindow::builder()
        .child(&tabs)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(true)
        .build();
    header.pack_start(&tabs_scroller);
    header.pack_start(&add_btn);
    header.pack_start(&add_revealer);
    header.pack_end(&fullscreen_btn);
    header.pack_end(&stats_btn);

    let stats = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .css_classes(["stats"])
        .build();
    stats_btn
        .bind_property("active", &stats, "visible")
        .sync_create()
        .build();
    let status = gtk::Label::builder()
        .label("Not connected")
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .can_target(false)
        .css_classes(["status-message"])
        .build();
    let hint = gtk::Label::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .visible(false)
        .can_target(false)
        .css_classes(["hint"])
        .build();

    // The video is a plain Picture given the whole allocation (Fill); the
    // paintable fits the frame using the surface device scale, see
    // paintable::layout.
    let picture = gtk::Picture::builder()
        .hexpand(true)
        .vexpand(true)
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Fill)
        .css_classes(["video"])
        .build();
    let frame = paintable::FramePaintable::default();
    picture.set_paintable(Some(&frame));
    let video: gtk::Widget = picture.clone().upcast();

    let transfers = Rc::new(clipboard_ui::TransferBars::new());
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&video));
    overlay.add_overlay(&stats);
    overlay.add_overlay(transfers.widget());
    overlay.add_overlay(&status);
    overlay.add_overlay(&hint);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&header);
    let capture_status = gtk::Label::builder()
        .wrap(true)
        .css_classes(["dim-label"])
        .build();
    content.append(&capture_status);
    content.append(&overlay);
    window.set_content(Some(&content));
    install_fullscreen_bars(
        &window,
        &content,
        &overlay,
        &header,
        &fullscreen_btn,
        &entry,
    );

    let ui = Rc::new(App {
        window: window.clone(),
        video: video.clone(),
        frame,
        stats: stats.clone(),
        status: status.clone(),
        hint,
        hint_generation: Cell::new(0),
        tabs,
        add_btn,
        add_revealer,
        entry: entry.clone(),
        transfers,
        remote_clipboard: Rc::default(),
        sessions: RefCell::new(Vec::new()),
        active: RefCell::new(None),
        view_size: Cell::new((0, 0)),
        view_scale: Cell::new(1.0),
        released: Cell::new(false),
        config_path: Config::default_path(),
        cli: cli.clone(),
        keymap: RefCell::new(keymap::Source::new(keyboard_policy(cli))),
        local_keys: RefCell::new(keyboard::LocalKeys::default()),
        closing: Cell::new(false),
        native_capture: RefCell::new(system_capture::Driver::new(cli.capture_system_shortcuts)),
        capture_status,
    });

    let hotkey = ReleaseHotkey::parse(&cli.release_hotkey).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "invalid --release-hotkey; shortcut release disabled");
        ReleaseHotkey::None
    });
    install_input_handlers(&ui, &video, &window, hotkey);
    install_native_capture_pump(&ui);
    install_resize_handler(&ui);
    install_address_bar(&ui, &connect_btn);
    refresh_tabs(&ui);

    // Fullscreen toggle.
    {
        let window = window.clone();
        fullscreen_btn.connect_toggled(move |b| {
            if b.is_active() {
                window.fullscreen();
            } else {
                window.unfullscreen();
            }
        });
    }

    let css = gtk::CssProvider::new();
    css.load_from_string(concat!(
        ".video { background: #000; }",
        ".status-message { color: rgba(255,255,255,0.7); font-size: 1.15em; margin: 24px; }",
        ".hint { background: rgba(0,0,0,0.7); color: #fff; padding: 6px 14px; border-radius: 999px; margin-bottom: 24px; }",
        ".stats { background: rgba(0,0,0,0.6); color: #fff; padding: 6px; margin: 6px; border-radius: 6px; font-family: monospace; }",
        ".transfers { margin: 6px; }",
        ".transfer { background: rgba(0,0,0,0.7); color: #fff; padding: 6px 8px; border-radius: 6px; }",
        // A tab is one pill: the name and its stop/forget icon carry no
        // backgrounds of their own, only the icon brightens under the pointer.
        ".machine-tab { border-radius: 8px; }",
        ".machine-tab:hover { background: alpha(currentColor, 0.07); }",
        ".machine-tab.active { background: alpha(currentColor, 0.14); }",
        ".machine-tab > .tab-label { padding: 5px 2px 5px 12px; font-weight: bold; }",
        ".machine-tab > button { background: none; border: none; outline: none; box-shadow: none; min-height: 0; }",
        ".machine-tab > button.tab-action { padding: 0; min-width: 22px; min-height: 22px; margin-right: 4px; color: alpha(currentColor, 0.55); -gtk-icon-size: 12px; }",
        ".machine-tab > button.tab-action:hover { color: currentColor; }",
        ".status-dot { min-width: 6px; min-height: 6px; border-radius: 50%; border: 1.5px solid alpha(currentColor, 0.6); }",
        ".status-dot.connected { background: var(--success-color); border-color: var(--success-color); }",
    ));
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    // Offer every local clipboard change to the servers; our own proxy for
    // a server's selection does not count as a change.
    {
        let ui = ui.clone();
        clipboard_ui::watch_local(ui.remote_clipboard.clone(), move || {
            if ui.cli.no_clipboard || ui.closing.get() {
                return Vec::new();
            }
            ui.sessions
                .borrow()
                .iter()
                .filter_map(|s| s.input_tx.borrow().clone())
                .collect()
        });
    }

    window.present();
    // Reconnect the machines that were connected when the window last
    // closed, then any named on the command line, which is shown first.
    let config = Config::load(&ui.config_path);
    let from_cli = endpoints_from_cli(cli);
    for machine in &config.open {
        open_session(&ui, ssh_endpoint(cli, machine));
    }
    for endpoint in from_cli.iter().cloned() {
        if find_session(&ui, machine_name(&endpoint)).is_none() {
            open_session(&ui, endpoint);
        }
    }
    // The address bar showed for the empty tabs; with sessions open it
    // folds into the +.
    show_address_bar(&ui, false);
    let first = from_cli
        .first()
        .map(|e| machine_name(e).to_string())
        .or(config.shown)
        .and_then(|name| find_session(&ui, &name))
        .or_else(|| ui.sessions.borrow().first().cloned());
    match first {
        Some(first) => show_session(&ui, &first),
        None if ui.tabs.first_child().is_none() => {
            entry.grab_focus();
        }
        None => {}
    }
}

const CONNECT_ICON: &str = "go-next-symbolic";
const STOP_ICON: &str = "media-playback-stop-symbolic";
const FORGET_ICON: &str = "window-close-symbolic";

/// Wire the address bar: the + expands it, Enter connects to what was
/// typed, and Escape, or leaving it empty, folds it back into the +.
fn install_address_bar(ui: &Rc<App>, connect_btn: &gtk::Button) {
    {
        let ui = ui.clone();
        ui.entry
            .clone()
            .connect_activate(move |_| connect_from_address_bar(&ui));
    }
    {
        let ui = ui.clone();
        connect_btn.connect_clicked(move |_| connect_from_address_bar(&ui));
    }
    {
        let ui = ui.clone();
        ui.add_btn.clone().connect_clicked(move |_| {
            show_address_bar(&ui, true);
            ui.entry.grab_focus();
        });
    }

    let focus = gtk::EventControllerFocus::new();
    {
        let ui = ui.clone();
        // Focus may be moving to the connect button, so decide once the new
        // focus is known.
        focus.connect_leave(move |_| {
            let ui = ui.clone();
            glib::idle_add_local_once(move || {
                let in_bar = gtk::prelude::GtkWindowExt::focus(&ui.window)
                    .is_some_and(|w| w.is_ancestor(&ui.add_revealer));
                if !in_bar && ui.entry.text().trim().is_empty() {
                    show_address_bar(&ui, false);
                }
            });
        });
    }
    ui.entry.add_controller(focus);

    let keys = gtk::EventControllerKey::new();
    {
        let ui = ui.clone();
        keys.connect_key_pressed(move |_, keyval, _, _| {
            if keyval == gdk::Key::Escape {
                cancel_address_edit(&ui);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }
    ui.entry.add_controller(keys);
}

/// Show the address bar in place of the +, or fold it back. With no tabs
/// the address bar is all there is, so it stays.
fn show_address_bar(ui: &App, show: bool) {
    let show = show || ui.tabs.first_child().is_none();
    ui.add_revealer.set_reveal_child(show);
    ui.add_btn.set_visible(!show);
    // A folded address bar must not keep the focus GTK gave it when the
    // window opened, or hovering the picture would never capture.
    if !show && address_bar_in_use(&ui.window, &ui.entry) {
        gtk::prelude::GtkWindowExt::set_focus(&ui.window, gtk::Widget::NONE);
    }
}

/// Connect to the machine in the address bar, or switch to its tab when it
/// is already connected.
fn connect_from_address_bar(ui: &Rc<App>) {
    let machine = ui.entry.text().trim().to_string();
    if machine.is_empty() {
        return;
    }
    ui.entry.set_text("");
    match find_session(ui, &machine) {
        Some(s) => show_session(ui, &s),
        None => open_session(ui, ssh_endpoint(&ui.cli, &machine)),
    }
    show_address_bar(ui, false);
    if active(ui).is_some() {
        ui.video.grab_focus();
    }
}

/// Give up the edit: empty the address bar, fold it back into the +, and
/// hand focus back to the remote screen when there is one.
fn cancel_address_edit(ui: &App) {
    ui.entry.set_text("");
    show_address_bar(ui, false);
    if active(ui).is_some() {
        ui.video.grab_focus();
    } else if ui.add_revealer.reveals_child() {
        ui.entry.grab_focus();
    } else {
        gtk::prelude::GtkWindowExt::set_focus(&ui.window, gtk::Widget::NONE);
    }
}

fn active(ui: &App) -> Option<Rc<Session>> {
    ui.active.borrow().clone()
}

fn is_active(ui: &App, s: &Rc<Session>) -> bool {
    ui.active
        .borrow()
        .as_ref()
        .is_some_and(|a| Rc::ptr_eq(a, s))
}

fn find_session(ui: &App, name: &str) -> Option<Rc<Session>> {
    ui.sessions
        .borrow()
        .iter()
        .find(|s| s.name == name)
        .cloned()
}

/// The tabs, in order: the remembered machines as arranged, then any session
/// not remembered yet (a first connect still in progress, or a dev
/// `--connect`).
fn tab_names(ui: &App) -> Vec<String> {
    let mut names = Config::load(&ui.config_path).machines;
    for s in ui.sessions.borrow().iter() {
        if !names.contains(&s.name) {
            names.push(s.name.clone());
        }
    }
    names
}

/// Rebuild the tabs from the config and the running sessions.
fn refresh_tabs(ui: &Rc<App>) {
    while let Some(tab) = ui.tabs.first_child() {
        ui.tabs.remove(&tab);
    }
    for name in tab_names(ui) {
        ui.tabs.append(&machine_tab(ui, &name));
    }
    // An empty scroller still claims its minimum width, which would push
    // the address bar away from where the first tab starts.
    let has_tabs = ui.tabs.first_child().is_some();
    if let Some(scroller) = ui.tabs.ancestor(gtk::ScrolledWindow::static_type()) {
        scroller.set_visible(has_tabs);
    }
    if !has_tabs {
        show_address_bar(ui, true);
    }
}

/// A machine's tab: its name, which shows that machine, and a button that
/// appears on hover: stop for a running session, forget for the rest. Tabs
/// can be dragged into a new order.
fn machine_tab(ui: &Rc<App>, name: &str) -> gtk::Box {
    let session = find_session(ui, name);
    let running = session.is_some();
    // The name is a plain label and the whole tab takes the click, so no
    // part of a tab takes keyboard focus and grows a focus ring.
    let label = gtk::Label::builder()
        .label(name)
        .css_classes(["tab-label"])
        .build();
    if !running {
        label.add_css_class("dim-label");
    }
    // The slot on the right shows whether the machine is connected, and
    // turns into its button while the pointer is on it.
    let connected = session.as_ref().is_some_and(|s| s.connected.get());
    let status_tip = if connected {
        "Connected"
    } else {
        "Not connected"
    };
    let action_tip = if running { "Disconnect" } else { "Forget" };
    let dot = gtk::Box::builder()
        .css_classes(["status-dot"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    if connected {
        dot.add_css_class("connected");
    }
    let slot = gtk::Stack::new();
    slot.add_named(&dot, Some("status"));
    slot.add_named(
        &gtk::Image::from_icon_name(if running { STOP_ICON } else { FORGET_ICON }),
        Some("action"),
    );
    let action = gtk::Button::builder()
        .child(&slot)
        .tooltip_text(status_tip)
        .css_classes(["tab-action"])
        .valign(gtk::Align::Center)
        .focusable(false)
        .build();
    let tab = gtk::Box::builder().css_classes(["machine-tab"]).build();
    if session.as_ref().is_some_and(|s| is_active(ui, s)) {
        tab.add_css_class("active");
    }
    tab.append(&label);
    tab.append(&action);

    // The dot turns into its button only under the pointer, so a click on
    // it always lands on what it shows.
    let hover = gtk::EventControllerMotion::new();
    {
        let slot = slot.clone();
        hover.connect_enter(move |c, _, _| {
            slot.set_visible_child_name("action");
            if let Some(button) = c.widget() {
                button.set_tooltip_text(Some(action_tip));
            }
        });
    }
    {
        let slot = slot.clone();
        hover.connect_leave(move |c| {
            slot.set_visible_child_name("status");
            if let Some(button) = c.widget() {
                button.set_tooltip_text(Some(status_tip));
            }
        });
    }
    action.add_controller(hover);

    // The handlers rebuild the tabs, so they run once the click is done
    // with the widget that received it. The action button claims its own
    // clicks, so this sees only the rest of the tab.
    let click = gtk::GestureClick::builder().button(1).build();
    {
        let ui = ui.clone();
        let name = name.to_string();
        click.connect_released(move |_, _, _, _| {
            let (ui, name) = (ui.clone(), name.clone());
            glib::idle_add_local_once(move || select_tab(&ui, &name));
        });
    }
    tab.add_controller(click);
    {
        let ui = ui.clone();
        let name = name.to_string();
        action.connect_clicked(move |_| {
            let (ui, name) = (ui.clone(), name.clone());
            glib::idle_add_local_once(move || {
                if running {
                    stop_session(&ui, &name);
                } else {
                    forget_machine(&ui, &name);
                }
            });
        });
    }

    // Drag to reorder. The source runs in the capture phase so a drag
    // starting on the name wins over the button's click and the header's
    // window move.
    let drag = gtk::DragSource::builder()
        .actions(gdk::DragAction::MOVE)
        .propagation_phase(gtk::PropagationPhase::Capture)
        .build();
    {
        let name = name.to_string();
        drag.connect_prepare(move |source, x, y| {
            if let Some(tab) = source.widget() {
                let icon = gtk::WidgetPaintable::new(Some(&tab));
                source.set_icon(Some(&icon), x as i32, y as i32);
            }
            Some(gdk::ContentProvider::for_value(&name.to_value()))
        });
    }
    tab.add_controller(drag);

    let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
    {
        let ui = ui.clone();
        let name = name.to_string();
        drop.connect_drop(move |target, value, x, _| {
            let Ok(dragged) = value.get::<String>() else {
                return false;
            };
            let after = target.widget().is_some_and(|w| x > w.width() as f64 / 2.0);
            let (ui, name) = (ui.clone(), name.clone());
            glib::idle_add_local_once(move || move_tab(&ui, &dragged, &name, after));
            true
        });
    }
    tab.add_controller(drop);
    tab
}

/// Show a machine's tab: switch to its running session, retrying one whose
/// reconnects gave up, or connect to it.
fn select_tab(ui: &Rc<App>, name: &str) {
    match find_session(ui, name) {
        Some(s) => {
            if s.gave_up.get() {
                s.retries.set(0);
                s.gave_up.set(false);
                start_session(ui.clone(), s.clone());
            }
            show_session(ui, &s);
        }
        None => open_session(ui, ssh_endpoint(&ui.cli, name)),
    }
}

/// Connect to `endpoint` in a new session and show it.
fn open_session(ui: &Rc<App>, endpoint: Endpoint) {
    let s = Rc::new(Session::new(endpoint));
    ui.sessions.borrow_mut().push(s.clone());
    start_session(ui.clone(), s.clone());
    show_session(ui, &s);
}

/// Make `s` the session the window shows and sends input to.
fn show_session(ui: &Rc<App>, s: &Rc<Session>) {
    if !is_active(ui, s) {
        ui.remote_clipboard.clear();
        if let Some(old) = active(ui) {
            release_session_keyboard(&old);
        }
        *ui.active.borrow_mut() = Some(s.clone());
        if matches!(s.endpoint, Endpoint::Ssh(_)) {
            let mut config = Config::load(&ui.config_path);
            config.shown = Some(s.name.clone());
            save_config(ui, &config);
        }
    }
    ui.window.set_title(Some(&window_title(&s.endpoint)));
    show_status(ui, &s.status.borrow());
    ui.stats.set_text(&s.stats.borrow());
    match &*s.last_frame.borrow() {
        Some((texture, view)) => ui.frame.set_frame(texture.clone(), device_scale(ui), *view),
        None => ui.frame.clear(),
    }
    ui.video.set_cursor(s.cursor.borrow().as_ref());
    // A background session kept the size the window had when it was last
    // shown.
    request_resize(ui);
    request_zoom(ui);
    refresh_tabs(ui);
}

/// Show no session: a black screen, as on a fresh window.
fn show_nothing(ui: &Rc<App>) {
    ui.remote_clipboard.clear();
    if let Some(old) = active(ui) {
        release_session_keyboard(&old);
    }
    ui.active.borrow_mut().take();
    ui.window.set_title(Some("Gliff"));
    show_status(ui, "Not connected");
    ui.stats.set_text("");
    ui.frame.clear();
    ui.video.set_cursor(None);
    refresh_tabs(ui);
}

/// End a machine's session. When it was the one shown, the window moves to
/// the nearest running tab, to the right first.
fn stop_session(ui: &Rc<App>, name: &str) {
    let Some(s) = find_session(ui, name) else {
        return;
    };
    let was_active = is_active(ui, &s);
    release_session_keyboard(&s);
    // Bumping the generation stops its pollers and pending reconnects, and
    // dropping the sender closes the worker's input, which ends it.
    s.generation.set(s.generation.get() + 1);
    s.input_tx.borrow_mut().take();
    s.last_frame.borrow_mut().take();
    // A machine stopped by hand stays stopped when the window reopens.
    if matches!(s.endpoint, Endpoint::Ssh(_)) {
        let mut config = Config::load(&ui.config_path);
        config.set_open(&s.name, false);
        save_config(ui, &config);
    }
    let names = tab_names(ui);
    ui.sessions.borrow_mut().retain(|o| !Rc::ptr_eq(o, &s));
    if !was_active {
        refresh_tabs(ui);
        return;
    }
    let at = names.iter().position(|n| *n == s.name).unwrap_or(0);
    let next = names[at + 1..]
        .iter()
        .chain(names[..at].iter().rev())
        .find_map(|n| find_session(ui, n));
    match next {
        Some(next) => show_session(ui, &next),
        None => show_nothing(ui),
    }
}

/// Drop a machine that is not running from the remembered list.
fn forget_machine(ui: &Rc<App>, name: &str) {
    let mut config = Config::load(&ui.config_path);
    config.forget(name);
    save_config(ui, &config);
    refresh_tabs(ui);
}

/// Move the dragged tab next to the one it was dropped on. Only remembered
/// machines have a place to keep.
fn move_tab(ui: &Rc<App>, dragged: &str, target: &str, after: bool) {
    let mut config = Config::load(&ui.config_path);
    if config.move_machine(dragged, target, after) {
        save_config(ui, &config);
        refresh_tabs(ui);
    }
}

fn save_config(ui: &App, config: &Config) {
    if let Err(e) = config.save(&ui.config_path) {
        tracing::warn!(path = %ui.config_path.display(), error = %e, "cannot save config");
    }
}

/// Set a session's status, showing it when the session is. A streaming
/// session has none.
fn set_status(ui: &App, s: &Rc<Session>, text: &str) {
    *s.status.borrow_mut() = text.to_string();
    if is_active(ui, s) {
        show_status(ui, text);
    }
}

fn show_status(ui: &App, text: &str) {
    ui.status.set_text(text);
    ui.status.set_visible(!text.is_empty());
}

/// Show a note near the bottom of the picture for a few seconds.
fn show_hint(ui: &Rc<App>, text: &str) {
    let generation = ui.hint_generation.get() + 1;
    ui.hint_generation.set(generation);
    ui.hint.set_text(text);
    ui.hint.set_visible(true);
    let ui = ui.clone();
    glib::timeout_add_local_once(Duration::from_millis(2500), move || {
        if ui.hint_generation.get() == generation {
            ui.hint.set_visible(false);
        }
    });
}

fn set_stats(ui: &App, s: &Rc<Session>, text: &str) {
    *s.stats.borrow_mut() = text.to_string();
    if is_active(ui, s) {
        ui.stats.set_text(text);
    }
}

/// Go back to a black screen with no stream geometry.
fn show_disconnected(ui: &Rc<App>, s: &Rc<Session>) {
    if is_active(ui, s) {
        ui.remote_clipboard.clear();
    }
    release_session_keyboard(s);
    *s.keyboard.borrow_mut() = keyboard::Keyboard::default();
    if s.connected.replace(false) {
        refresh_tabs(ui);
    }
    s.last_frame.borrow_mut().take();
    s.stream_size.set((0, 0));
    s.stream_view.set((0, 0));
    s.server_view.set((0, 0));
    s.cursor.borrow_mut().take();
    set_stats(ui, s, "");
    if is_active(ui, s) {
        ui.frame.clear();
        ui.video.set_cursor(None);
    }
}

/// Start a worker for the session. Each call is a new generation; pollers
/// and reconnects from an older one stop themselves.
fn start_session(ui: Rc<App>, s: Rc<Session>) {
    if ui.closing.get() {
        return;
    }
    if is_active(&ui, &s) {
        ui.remote_clipboard.clear();
    }
    release_session_keyboard(&s);
    *s.keyboard.borrow_mut() = keyboard::Keyboard::default();
    let (frame_tx, frame_rx) = sync_channel::<Picture>(2);
    let (status_tx, status_rx) = channel::<Status>();
    let (input_tx, input_rx) = input_queue::channel();
    // Replacing the sender closes the old worker's input, which ends it.
    *s.input_tx.borrow_mut() = Some(input_tx);
    s.zoom.set(1);
    s.reset_resize();
    let generation = s.generation.get() + 1;
    s.generation.set(generation);
    set_status(&ui, &s, "Connecting…");

    let endpoint = s.endpoint.clone();
    let keymap = ui.keymap.borrow().text.clone();
    std::thread::Builder::new()
        .name("gliff-net".into())
        .spawn(move || {
            Worker {
                endpoint,
                frames: frame_tx,
                status: status_tx,
                input: input_rx,
                keymap,
            }
            .run();
        })
        .expect("spawn network thread");

    poll_frames(ui.clone(), s.clone(), frame_rx, generation);
    poll_status(ui, s, status_rx, generation);
}

const MAX_RETRIES: u32 = 5;

/// Add the session's machine to the remembered list, and to the ones to
/// reconnect when the window opens again, once per session. The connected
/// status repeats on every stream reconfigure, such as a resize.
fn remember_machine(ui: &Rc<App>, s: &Session) {
    if !matches!(s.endpoint, Endpoint::Ssh(_)) || s.remembered.replace(true) {
        return;
    }
    let mut config = Config::load(&ui.config_path);
    config.remember(&s.name);
    config.set_open(&s.name, true);
    save_config(ui, &config);
    refresh_tabs(ui);
}

/// Schedule a reconnect after a short delay, unless we have exhausted retries.
fn schedule_reconnect(ui: Rc<App>, s: Rc<Session>, generation: u64) {
    if ui.closing.get() {
        return;
    }
    let n = s.retries.get() + 1;
    s.retries.set(n);
    if n > MAX_RETRIES {
        set_status(&ui, &s, "Disconnected — click the tab to reconnect");
        s.gave_up.set(true);
        return;
    }
    set_status(&ui, &s, &format!("Reconnecting… (attempt {n})"));
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        if s.generation.get() == generation {
            start_session(ui, s);
        }
    });
}

/// Pull decoded frames on the GTK main loop, latest-wins. The shown session
/// paints them; every session keeps its latest for when its tab is shown.
fn poll_frames(ui: Rc<App>, s: Rc<Session>, rx: Receiver<Picture>, generation: u64) {
    glib::timeout_add_local(Duration::from_millis(8), move || {
        if s.generation.get() != generation {
            return glib::ControlFlow::Break;
        }
        let mut latest = None;
        loop {
            match rx.try_recv() {
                Ok(f) => latest = Some(f),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                // The worker ended; stop this per-session timer so it does not
                // accumulate across reconnects.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return glib::ControlFlow::Break
                }
            }
        }
        if let Some(p) = latest {
            match frame_texture(p.frame) {
                Ok(texture) => {
                    // Geometry follows the frame that really paints, so the
                    // pointer never maps against a picture that is not on
                    // screen (a newer config, or a failed import).
                    s.stream_size.set(p.stream);
                    s.stream_view.set(p.view);
                    s.stream_scale.set((p.scale_milli.max(1) as f32) / 1000.0);
                    if is_active(&ui, &s) {
                        ui.frame
                            .set_frame(texture.clone(), device_scale(&ui), p.view);
                    }
                    *s.last_frame.borrow_mut() = Some((texture, p.view));
                }
                Err(e) => tracing::warn!(error = %e, "frame texture import failed"),
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Wrap owned native-decoded BGRA pixels as a GDK memory texture.
fn frame_texture(f: Frame) -> Result<gdk::Texture, glib::Error> {
    match f {
        Frame::Bgra(f) => {
            let bytes = glib::Bytes::from_owned(f.pixels);
            Ok(gdk::MemoryTexture::new(
                f.width as i32,
                f.height as i32,
                gdk::MemoryFormat::B8g8r8x8,
                &bytes,
                f.width as usize * 4,
            )
            .upcast())
        }
    }
}

fn poll_status(ui: Rc<App>, s: Rc<Session>, rx: Receiver<Status>, generation: u64) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        if s.generation.get() != generation {
            return glib::ControlFlow::Break;
        }
        while let Ok(status) = rx.try_recv() {
            match status {
                // Pointer-mapping geometry (stream_size/view/scale) is not
                // read from the config: it follows each decoded frame, so a
                // new config never remaps clicks against the old picture.
                Status::Connected {
                    video,
                    view_width,
                    view_height,
                    fps_cap,
                    headless,
                } => {
                    s.server_view.set((view_width, view_height));
                    s.fps_cap.set(fps_cap);
                    s.headless.set(headless);
                    s.resize_requested.set((0, 0));
                    s.retries.set(0);
                    s.gave_up.set(false);
                    if !s.connected.replace(true) {
                        refresh_tabs(&ui);
                    }
                    set_status(&ui, &s, "");
                    // Visible before the first per-second stats arrive.
                    set_stats(&ui, &s, &video);
                    remember_machine(&ui, &s);
                    // A fresh server starts at its own default size; a
                    // reconnect must bring it back to the window. The gate
                    // compares against the view, so a server-chosen
                    // reduced-resolution stream never triggers one. A
                    // background session is resized when it is shown.
                    if is_active(&ui, &s) {
                        request_resize(&ui);
                        if headless && s.cursor.borrow().is_none() {
                            let cursor = hidden_cursor();
                            ui.video.set_cursor(Some(&cursor));
                            *s.cursor.borrow_mut() = Some(cursor);
                        }
                    }
                }
                Status::Stats {
                    fps,
                    mbit,
                    decode_ms,
                    video,
                } => {
                    let (sw, sh) = s.stream_size.get();
                    let (vw, vh) = s.stream_view.get();
                    let stream = if (sw, sh) == (vw, vh) {
                        format!("{sw}x{sh}")
                    } else {
                        format!("{sw}x{sh} → {vw}x{vh}")
                    };
                    let cap = s.fps_cap.get();
                    set_stats(
                        &ui,
                        &s,
                        &format!(
                            "{video}  {fps:.0} fps  {mbit:.1} Mbit/s  decode {decode_ms:.1} ms  {stream} @{cap}"
                        ),
                    );
                }
                Status::Cursor {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                } => set_remote_cursor(
                    &ui,
                    &s,
                    CursorImage {
                        width,
                        height,
                        hot_x,
                        hot_y,
                        argb: &argb,
                    },
                ),
                // Only the shown machine may take over the local clipboard.
                Status::ClipboardOffer {
                    serial,
                    mime_types,
                    files,
                } => {
                    if !ui.cli.no_clipboard && is_active(&ui, &s) {
                        if let Some(tx) = s.input_tx.borrow().as_ref() {
                            let weak = Rc::downgrade(&ui);
                            clipboard_ui::set_remote_offer(
                                &ui.remote_clipboard,
                                tx,
                                serial,
                                mime_types,
                                files,
                                move |message| {
                                    if let Some(ui) = weak.upgrade() {
                                        show_hint(&ui, message);
                                    }
                                },
                            );
                        }
                    }
                }
                Status::ClipboardRead { mime_type, reply } => {
                    if !ui.cli.no_clipboard {
                        clipboard_ui::read_local(ui.remote_clipboard.clone(), mime_type, reply)
                    }
                }
                Status::ClipboardTransfer { id, progress } => {
                    let cancel_via = s.clone();
                    ui.transfers.update(id, &progress, move |id| {
                        if let Some(tx) = cancel_via.input_tx.borrow().clone() {
                            let _ = tx.send(clipboard::ToWorker::CancelTransfer(id));
                        }
                    });
                }
                Status::Error(e) => {
                    tracing::error!(machine = %s.name, error = %e, "connection failed");
                    set_status(&ui, &s, &format!("Error: {e}"));
                    show_disconnected(&ui, &s);
                    schedule_reconnect(ui.clone(), s.clone(), generation);
                    return glib::ControlFlow::Break;
                }
                Status::Incompatible(message) => {
                    tracing::error!(machine = %s.name, error = %message, "incompatible server");
                    set_status(&ui, &s, &message);
                    show_disconnected(&ui, &s);
                    s.gave_up.set(true);
                    return glib::ControlFlow::Break;
                }
                Status::Closed => {
                    show_disconnected(&ui, &s);
                    schedule_reconnect(ui.clone(), s.clone(), generation);
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    });
}

/// Show the remote cursor as the video widget's own cursor, so the local
/// compositor draws it at the real pointer position with no added latency.
/// An image with no visible shape (fully transparent, or one flat colour as
/// Hyprland sends when it has no cursor image to share) falls back to the
/// default pointer — except on a headless session, whose frames already
/// carry the compositor-drawn cursor and would show two pointers.
fn set_remote_cursor(ui: &App, s: &Rc<Session>, image: CursorImage) {
    let cursor = remote_cursor(image).or_else(|| s.headless.get().then(hidden_cursor));
    if is_active(ui, s) {
        ui.video.set_cursor(cursor.as_ref());
    }
    *s.cursor.borrow_mut() = cursor;
}

/// A fully transparent 1×1 cursor: the compositor shows nothing for the
/// local pointer while the video carries the remote one.
fn hidden_cursor() -> gdk::Cursor {
    let bytes = glib::Bytes::from_owned(vec![0u8; 4]);
    let texture = gdk::MemoryTexture::new(1, 1, gdk::MemoryFormat::B8g8r8a8, &bytes, 4);
    gdk::Cursor::from_texture(&texture, 0, 0, None)
}

struct CursorImage<'a> {
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
    argb: &'a [u8],
}

fn remote_cursor(image: CursorImage) -> Option<gdk::Cursor> {
    let CursorImage {
        width,
        height,
        hot_x,
        hot_y,
        argb,
    } = image;
    let needed = width as u64 * height as u64 * 4;
    if width == 0 || height == 0 || width > 1024 || height > 1024 || (argb.len() as u64) < needed {
        return None;
    }
    if !has_visible_shape(argb) {
        return None;
    }
    let bytes = glib::Bytes::from(argb);
    let texture = gdk::MemoryTexture::new(
        width as i32,
        height as i32,
        gdk::MemoryFormat::B8g8r8a8,
        &bytes,
        width as usize * 4,
    );
    Some(gdk::Cursor::from_texture(&texture, hot_x, hot_y, None))
}

fn has_visible_shape(argb: &[u8]) -> bool {
    let mut pixels = argb.as_chunks::<4>().0.iter();
    let Some(first) = pixels.next() else {
        return false;
    };
    let opaque = first[3] != 0 || pixels.clone().any(|p| p[3] != 0);
    let uniform = pixels.all(|p| p == first);
    opaque && !uniform
}

/// Map a widget-space point to the remote output's logical coordinates:
/// invert the frame layout to view pixels, scale to physical stream pixels
/// (a reduced-resolution stream is stretched to the view box), then divide
/// by the remote scale, which is what the virtual pointer expects.
fn to_remote(ui: &App, x: f64, y: f64) -> (f64, f64) {
    let Some(s) = active(ui) else {
        return (0.0, 0.0);
    };
    let (rw, rh) = s.stream_size.get();
    let (vw, vh) = s.stream_view.get();
    if rw == 0 || rh == 0 {
        return (0.0, 0.0);
    }
    let device = device_scale(ui);
    let (aw, ah) = (ui.video.width() as f64, ui.video.height() as f64);
    let Some(l) = paintable::layout((vw, vh), device, aw, ah) else {
        return (0.0, 0.0);
    };
    let vx = (x - l.x) / l.factor * device;
    let vy = (y - l.y) / l.factor * device;
    let px = (vx * rw as f64 / vw as f64).clamp(0.0, rw as f64);
    let py = (vy * rh as f64 / vh as f64).clamp(0.0, rh as f64);
    let scale = s.stream_scale.get().max(0.01) as f64;
    (px / scale, py / scale)
}

fn send(ui: &App, msg: ClientMsg) {
    if let Some(s) = active(ui) {
        send_to(&s, msg);
    }
}

fn send_to(s: &Session, msg: ClientMsg) {
    if let Some(tx) = s.input_tx.borrow().as_ref() {
        let _ = tx.send(clipboard::ToWorker::Send(msg));
    }
}

/// How the user releases captured shortcuts back to the local compositor.
#[derive(Clone)]
enum ReleaseHotkey {
    None,
    DoubleTap {
        keyval: gdk::Key,
        within: Duration,
    },
    Chord {
        mods: gdk::ModifierType,
        keyval: gdk::Key,
    },
}

const CHORD_MODS: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK
    .union(gdk::ModifierType::ALT_MASK)
    .union(gdk::ModifierType::SHIFT_MASK)
    .union(gdk::ModifierType::SUPER_MASK);

impl ReleaseHotkey {
    fn parse(spec: &str) -> Result<Self, String> {
        let s = spec.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        let lower = s.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("double-")
            .or_else(|| lower.strip_prefix("double:"))
        {
            return Ok(Self::DoubleTap {
                keyval: key_from_name(rest)?,
                within: Duration::from_millis(400),
            });
        }
        let mut mods = gdk::ModifierType::empty();
        let mut keyval = None;
        for tok in s.split('+') {
            match tok.trim().to_ascii_lowercase().as_str() {
                "" => {}
                "ctrl" | "control" => mods |= gdk::ModifierType::CONTROL_MASK,
                "alt" => mods |= gdk::ModifierType::ALT_MASK,
                "shift" => mods |= gdk::ModifierType::SHIFT_MASK,
                "super" | "logo" | "win" | "meta" => mods |= gdk::ModifierType::SUPER_MASK,
                other => keyval = Some(key_from_name(other)?),
            }
        }
        match keyval {
            Some(keyval) => Ok(Self::Chord { mods, keyval }),
            None => Err(format!("no key in release hotkey '{spec}'")),
        }
    }

    fn matches_event(
        &self,
        reported: gdk::Key,
        unmodified: Option<gdk::Key>,
        state: gdk::ModifierType,
        last_tap: &RefCell<Option<Instant>>,
    ) -> bool {
        // Match the layout's base identity, not Shift/Option's resulting text.
        // Special/unmapped keys may not have a translation; keep their reported
        // identity rather than silently losing a usable release trigger.
        let key = unmodified
            .filter(|k| *k != gdk::Key::VoidSymbol)
            .unwrap_or(reported);
        self.matches(key, state, last_tap)
    }

    /// True if this press is the release trigger. For a double-tap it records
    /// the tap time and returns true only on the quick second press, so the
    /// first tap still reaches the remote.
    fn matches(
        &self,
        keyval: gdk::Key,
        state: gdk::ModifierType,
        last_tap: &RefCell<Option<Instant>>,
    ) -> bool {
        match self {
            Self::None => false,
            Self::Chord { mods, keyval: k } => {
                keyval == *k && normalize_hotkey_mods(state) == *mods
            }
            Self::DoubleTap { keyval: k, within } => {
                if keyval != *k {
                    return false;
                }
                let now = Instant::now();
                let mut lt = last_tap.borrow_mut();
                match *lt {
                    Some(prev) if now.duration_since(prev) <= *within => {
                        *lt = None;
                        true
                    }
                    _ => {
                        *lt = Some(now);
                        false
                    }
                }
            }
        }
    }
}

/// Resolve a key name to a `gdk::Key`, accepting lower-case and a few aliases.
fn key_from_name(name: &str) -> Result<gdk::Key, String> {
    let n = name.trim();
    let alias = match n.to_ascii_lowercase().as_str() {
        "esc" => Some("Escape"),
        "enter" | "return" => Some("Return"),
        "space" => Some("space"),
        _ => None,
    };
    let title = {
        let mut c = n.chars();
        c.next()
            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
            .unwrap_or_default()
    };
    for cand in alias.iter().copied().chain([n, title.as_str()]) {
        if !cand.is_empty() {
            if let Some(k) = gdk::Key::from_name(cand) {
                return Ok(k);
            }
        }
    }
    Err(format!("unknown key '{name}'"))
}

/// True while the address bar shows and holds the focus. GTK4 focuses the
/// text inside the entry, so ask the window for the focus widget and walk
/// up; a folded address bar is unmapped.
fn address_bar_in_use(window: &adw::ApplicationWindow, entry: &gtk::Entry) -> bool {
    entry.is_mapped()
        && gtk::prelude::GtkWindowExt::focus(window)
            .is_some_and(|f| f == *entry || f.is_ancestor(entry))
}

fn release_capture(ui: &Rc<App>, window: &adw::ApplicationWindow) {
    ui.released.set(true);
    if let Some(s) = active(ui) {
        release_session_keyboard(&s);
    }
    gtk::prelude::GtkWindowExt::set_focus(window, gtk::Widget::NONE);
    show_hint(ui, "Focus released — click the screen to focus again");
}

fn keyboard_policy(cli: &Cli) -> keyboard::Policy {
    keyboard::Policy {
        command: cli.command_key,
        option: cli.option_key,
    }
}

fn normalize_hotkey_mods(mut state: gdk::ModifierType) -> gdk::ModifierType {
    if state.contains(gdk::ModifierType::META_MASK) {
        state |= gdk::ModifierType::SUPER_MASK;
    }
    state & CHORD_MODS
}

fn send_keyboard_events(s: &Session, events: Vec<keyboard::Event>) {
    for event in events {
        send_to(
            s,
            ClientMsg::Key {
                keycode: event.code,
                pressed: event.pressed,
            },
        );
    }
}

fn release_session_keyboard(s: &Session) {
    s.keyboard.disarm_native();
    let events = s.keyboard.borrow_mut().release_all();
    send_keyboard_events(s, events);
}

fn video_owns_keyboard(ui: &App) -> bool {
    ui.window.is_active() && ui.video.has_focus() && !ui.released.get() && !ui.closing.get()
}

fn keyboard_target(ui: &App) -> Option<Rc<Session>> {
    if !video_owns_keyboard(ui) || ui.keymap.borrow().text.is_empty() {
        return None;
    }
    active(ui).filter(|s| s.connected.get())
}

fn sync_keyboard(ui: &App, s: &Session) {
    sync_keyboard_snapshot(
        s,
        macos_keyboard::modifiers(),
        macos_keyboard::caps(),
        keyboard_policy(&ui.cli),
    );
}

fn sync_keyboard_snapshot(s: &Session, modifiers: [bool; 8], caps: bool, policy: keyboard::Policy) {
    let mut events = s.keyboard.borrow_mut().sync_modifiers(modifiers, policy);
    events.extend(s.keyboard.borrow_mut().sync_caps(caps));
    send_keyboard_events(s, events);
}

fn refresh_keyboard_layout(ui: &App) {
    let changed = ui.keymap.borrow_mut().refresh();
    if changed {
        let keymap = ui.keymap.borrow().text.clone();
        for s in ui.sessions.borrow().iter() {
            release_session_keyboard(s);
            // FIFO with releases/keys, not a racing watch channel. New workers
            // use the same text in Hello; connected workers receive Keymap.
            if !keymap.is_empty() {
                let tx = s.input_tx.borrow().clone();
                if tx.is_some_and(|tx| {
                    tx.send(clipboard::ToWorker::Send(ClientMsg::Keymap {
                        keymap: keymap.clone(),
                    }))
                    .is_err()
                }) {
                    // Do not send later keys using a map the server never got.
                    // Closing this generation lets the usual reconnect send
                    // the current map in Hello, after queued releases/Bye.
                    s.input_tx.borrow_mut().take();
                    s.connected.set(false);
                    if is_active(ui, s) {
                        ui.remote_clipboard.clear();
                    }
                    set_status(ui, s, "Input queue full during layout change; reconnecting");
                }
            }
        }
    }
}

/// Prepare the configurable hotkey on GTK's main thread. The native callback
/// receives only physical code identities, not GDK/AppKit objects or text.
fn native_hotkey(cli: &Cli) -> system_capture::Hotkey {
    let Ok(hotkey) = ReleaseHotkey::parse(&cli.release_hotkey) else {
        return system_capture::Hotkey::default();
    };
    let (key, mods, double) = match hotkey {
        ReleaseHotkey::None => return system_capture::Hotkey::default(),
        ReleaseHotkey::DoubleTap { keyval, .. } => (keyval, gdk::ModifierType::empty(), true),
        ReleaseHotkey::Chord { keyval, mods } => (keyval, mods, false),
    };
    let mut flags = 0;
    for (gdk, native) in [
        (gdk::ModifierType::SHIFT_MASK, system_capture::SHIFT),
        (gdk::ModifierType::CONTROL_MASK, system_capture::CONTROL),
        (gdk::ModifierType::ALT_MASK, system_capture::OPTION),
        (gdk::ModifierType::SUPER_MASK, system_capture::COMMAND),
    ] {
        if mods.contains(gdk) {
            flags |= native;
        }
    }
    let codes = keyboard::KEYS
        .iter()
        .filter_map(|&(code, _)| (keymap::unmodified_keyval(code) == Some(key)).then_some(code))
        .collect();
    system_capture::Hotkey::new(codes, flags, double)
}

fn arm_native_capture(ui: &Rc<App>) {
    if !ui.cli.capture_system_shortcuts {
        return;
    }
    refresh_keyboard_layout(ui);
    let Some(s) = keyboard_target(ui) else {
        return;
    };
    let Some(tx) = s.input_tx.borrow().clone().filter(|tx| !tx.is_closed()) else {
        return;
    };
    if ui.native_capture.borrow().owns(&s.keyboard, &tx) {
        return;
    }
    ui.native_capture.borrow().disarm();
    release_session_keyboard(&s);
    // Queue held modifiers before the PointerButton-down that follows this
    // first/re-capture click. Repeated active clicks must not reset them.
    sync_keyboard(ui, &s);
    let target = system_capture::Target::new(
        tx,
        s.keyboard.clone(),
        keyboard_policy(&ui.cli),
        native_hotkey(&ui.cli),
        ui.local_keys.borrow().held(),
    );
    let armed = ui.native_capture.borrow().arm(target);
    if !armed {
        show_hint(ui, &ui.native_capture.borrow().status());
    }
}

fn install_native_capture_pump(ui: &Rc<App>) {
    ui.capture_status
        .set_text(&ui.native_capture.borrow().status());
    if ui.cli.capture_system_shortcuts {
        tracing::warn!(
            status = %ui.native_capture.borrow().status(),
            "Native capture requested; permission is never granted automatically"
        );
    }
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(10), move || {
        if ui.closing.get() {
            return glib::ControlFlow::Break;
        }
        let notice = ui.native_capture.borrow().notice();
        if let Some(reason) = notice {
            // Once per latched release, not per tick or key. The AX callback
            // publishes only an enum/code; logging stays on GTK's main thread.
            tracing::info!(
                ?reason,
                ui_snapshot = "before_notice_cleanup",
                foreground_check = ?ui.native_capture.borrow().foreground_check(),
                window_active = ui.window.is_active(),
                video_focus = ui.video.has_focus(),
                released = ui.released.get(),
                closing = ui.closing.get(),
                connected = active(&ui).is_some_and(|s| s.connected.get()),
                keymap_available = !ui.keymap.borrow().text.is_empty(),
                "Native capture released"
            );
            if reason == system_capture::Reason::Emergency {
                release_capture(&ui, &ui.window);
            } else if !matches!(
                reason,
                system_capture::Reason::Idle | system_capture::Reason::Focus
            ) {
                show_hint(
                    &ui,
                    "Native capture failed open; GTK normal. Click explicitly to retry.",
                );
            }
        }
        if let Some(s) = keyboard_target(&ui) {
            ui.native_capture.borrow().heartbeat(&s.keyboard);
        } else {
            ui.native_capture.borrow().disarm();
        }
        ui.capture_status
            .set_text(&ui.native_capture.borrow().status());
        glib::ControlFlow::Continue
    });
}

fn install_input_handlers(
    ui: &Rc<App>,
    video: &gtk::Widget,
    window: &adw::ApplicationWindow,
    hotkey: ReleaseHotkey,
) {
    video.set_focusable(true);
    video.set_can_focus(true);

    // Capture on the window to observe releases after focus moves to an entry.
    // Only the video owns remote input; local controls continue normally.
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let last_tap: Rc<RefCell<Option<Instant>>> = Rc::new(RefCell::new(None));
    {
        let ui = ui.clone();
        let window = window.clone();
        key.connect_key_pressed(move |_, keyval, code, state| {
            if ui.native_capture.borrow().active() {
                return glib::Propagation::Stop;
            }
            let fresh = ui.local_keys.borrow_mut().press(code);
            if !video_owns_keyboard(&ui) {
                return glib::Propagation::Proceed;
            }
            refresh_keyboard_layout(&ui);
            if fresh
                && hotkey.matches_event(keyval, keymap::unmodified_keyval(code), state, &last_tap)
            {
                release_capture(&ui, &window);
                return glib::Propagation::Stop;
            }
            if let Some(s) = keyboard_target(&ui) {
                sync_keyboard(&ui, &s);
                if fresh && !keyboard::MODIFIERS.contains(&code) && code != 57 {
                    let events = s
                        .keyboard
                        .borrow_mut()
                        .press(code, keyboard_policy(&ui.cli));
                    send_keyboard_events(&s, events);
                }
            } else {
                show_hint(
                    &ui,
                    "Keyboard unavailable: no supported macOS layout or connected session",
                );
            }
            glib::Propagation::Stop
        });
    }
    {
        let ui = ui.clone();
        key.connect_key_released(move |_, _, code, _| {
            if ui.native_capture.borrow().active() {
                return;
            }
            ui.local_keys.borrow_mut().release(code);
            if let Some(s) = keyboard_target(&ui) {
                sync_keyboard(&ui, &s);
                // Modifier state comes from the native snapshot, not GDK's
                // ambiguous aggregate flagsChanged release classification.
                if !keyboard::MODIFIERS.contains(&code) && code != 57 {
                    let events = s.keyboard.borrow_mut().release(code);
                    send_keyboard_events(&s, events);
                }
            }
        });
    }
    window.add_controller(key);
    let focus = gtk::EventControllerFocus::new();
    {
        let ui = ui.clone();
        focus.connect_leave(move |_| {
            if let Some(s) = active(&ui) {
                release_session_keyboard(&s);
            }
        });
    }
    {
        let ui = ui.clone();
        focus.connect_enter(move |_| {
            refresh_keyboard_layout(&ui);
            if ui.window.is_active() {
                ui.local_keys.borrow_mut().capture(
                    keyboard::KEYS
                        .iter()
                        .map(|&(code, _)| code)
                        .filter(|&code| macos_keyboard::down(code)),
                );
            }
            if let Some(s) = keyboard_target(&ui) {
                sync_keyboard(&ui, &s);
            }
        });
    }
    video.add_controller(focus);
    {
        let ui = ui.clone();
        window.connect_is_active_notify(move |window| {
            if !window.is_active() {
                if let Some(s) = active(&ui) {
                    release_session_keyboard(&s);
                }
            } else if ui.video.has_focus() {
                ui.local_keys.borrow_mut().capture(
                    keyboard::KEYS
                        .iter()
                        .map(|&(code, _)| code)
                        .filter(|&code| macos_keyboard::down(code)),
                );
            }
        });
    }
    // Refresh layout even on a quiet stream. Snapshot only already-sent keys,
    // and only with active video focus, to recover key-ups AppKit swallowed
    // during Command shortcuts without monitoring unrelated typing.
    {
        let ui = ui.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            if ui.closing.get() {
                return glib::ControlFlow::Break;
            }
            refresh_keyboard_layout(&ui);
            if let Some(s) = keyboard_target(&ui).filter(|_| !ui.native_capture.borrow().active()) {
                sync_keyboard(&ui, &s);
                let local = ui.local_keys.borrow().held();
                for code in local {
                    if !macos_keyboard::down(code) {
                        ui.local_keys.borrow_mut().release(code);
                    }
                }
                let held = s.keyboard.borrow().held_native();
                for code in held {
                    if !macos_keyboard::down(code) {
                        let events = s.keyboard.borrow_mut().release(code);
                        send_keyboard_events(&s, events);
                        ui.local_keys.borrow_mut().release(code);
                    }
                }
            }
            glib::ControlFlow::Continue
        });
    }
    {
        let ui = ui.clone();
        window.connect_close_request(move |_| {
            if ui.closing.replace(true) {
                return glib::Propagation::Proceed;
            }
            ui.remote_clipboard.clear();
            ui.native_capture.borrow_mut().shutdown();
            for s in ui.sessions.borrow().iter() {
                release_session_keyboard(s);
                s.input_tx.borrow_mut().take();
            }
            let ui = ui.clone();
            let deadline = Instant::now() + Duration::from_secs(2);
            glib::timeout_add_local(Duration::from_millis(25), move || {
                if ui.sessions.borrow().iter().all(|s| !s.connected.get())
                    || Instant::now() >= deadline
                {
                    ui.window.close();
                    glib::ControlFlow::Break
                } else {
                    glib::ControlFlow::Continue
                }
            });
            glib::Propagation::Stop
        });
    }

    // Pointer motion, and the capture itself: the remote gets the keyboard
    // only while the pointer is over the picture, so the header bar and
    // anything else outside it stay local.
    let motion = gtk::EventControllerMotion::new();
    {
        let ui = ui.clone();
        motion.connect_motion(move |_, x, y| {
            let (rx, ry) = to_remote(&ui, x, y);
            send(&ui, ClientMsg::PointerMotion { x: rx, y: ry });
        });
    }
    {
        let ui = ui.clone();
        let video = video.clone();
        motion.connect_enter(move |_, _, _| {
            if active(&ui).is_some()
                && !ui.released.get()
                && !address_bar_in_use(&ui.window, &ui.entry)
            {
                video.grab_focus();
            }
        });
    }
    {
        let ui = ui.clone();
        let window = window.clone();
        motion.connect_leave(move |_| {
            // A deliberate release lasts only as long as the pointer rests on
            // the picture; leaving and coming back captures again.
            ui.released.set(false);
            if gtk::prelude::GtkWindowExt::focus(&window).is_some_and(|f| f == ui.video) {
                gtk::prelude::GtkWindowExt::set_focus(&window, gtk::Widget::NONE);
            }
        });
    }
    video.add_controller(motion);

    // Buttons.
    let click = gtk::GestureClick::new();
    click.set_button(0); // any button
    {
        let ui = ui.clone();
        let video = video.clone();
        click.connect_pressed(move |g, _, _, _| {
            ui.released.set(false);
            video.grab_focus();
            arm_native_capture(&ui);
            send(
                &ui,
                ClientMsg::PointerButton {
                    button: evdev_button(g.current_button()),
                    pressed: true,
                },
            );
        });
    }
    {
        let ui = ui.clone();
        click.connect_released(move |g, _, _, _| {
            send(
                &ui,
                ClientMsg::PointerButton {
                    button: evdev_button(g.current_button()),
                    pressed: false,
                },
            );
        });
    }
    video.add_controller(click);

    // Scroll.
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    {
        let ui = ui.clone();
        scroll.connect_scroll(move |_, dx, dy| {
            if dy != 0.0 {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis: Axis::Vertical,
                        value: dy * 15.0,
                        discrete: Some(dy.signum() as i32),
                        stop: false,
                    },
                );
            }
            if dx != 0.0 {
                send(
                    &ui,
                    ClientMsg::PointerAxis {
                        axis: Axis::Horizontal,
                        value: dx * 15.0,
                        discrete: Some(dx.signum() as i32),
                        stop: false,
                    },
                );
            }
            glib::Propagation::Stop
        });
    }
    video.add_controller(scroll);

    // System-shortcut capture needs a macOS backend and is intentionally absent.
}

/// In fullscreen the header leaves the layout, so the picture gets the whole
/// screen, and slides in over it while the pointer is at the top edge. The header stays while the address bar is in use, so
/// it does not slide away under a half-typed machine.
fn install_fullscreen_bars(
    window: &adw::ApplicationWindow,
    content: &gtk::Box,
    overlay: &gtk::Overlay,
    header: &adw::HeaderBar,
    fullscreen_btn: &gtk::ToggleButton,
    entry: &gtk::Entry,
) {
    let address_bar_in_use = {
        let (window, entry) = (window.clone(), entry.clone());
        Rc::new(move || address_bar_in_use(&window, &entry))
    };
    let top = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .valign(gtk::Align::Start)
        .build();
    overlay.add_overlay(&top);

    {
        let (content, header) = (content.clone(), header.clone());
        let (top, fullscreen_btn) = (top.clone(), fullscreen_btn.clone());
        window.connect_fullscreened_notify(move |w| {
            let full = w.is_fullscreen();
            if fullscreen_btn.is_active() != full {
                fullscreen_btn.set_active(full);
            }
            if full {
                content.remove(&header);
                top.set_child(Some(&header));
            } else {
                top.set_reveal_child(false);
                top.set_child(None::<&gtk::Widget>);
                content.prepend(&header);
            }
        });
    }

    const EDGE: f64 = 2.0;
    const LEAVE_MARGIN: f64 = 8.0;
    let motion = gtk::EventControllerMotion::new();
    {
        let (window, header, top) = (window.clone(), header.clone(), top.clone());
        let address_bar_in_use = address_bar_in_use.clone();
        motion.connect_motion(move |_, _, y| {
            if !window.is_fullscreen() {
                return;
            }
            if y <= EDGE {
                top.set_reveal_child(true);
            } else if top.reveals_child()
                && y > header.height() as f64 + LEAVE_MARGIN
                && !address_bar_in_use()
            {
                top.set_reveal_child(false);
            }
        });
    }
    {
        let top = top.clone();
        let address_bar_in_use = address_bar_in_use.clone();
        motion.connect_leave(move |_| {
            if !address_bar_in_use() {
                top.set_reveal_child(false);
            }
        });
    }
    overlay.add_controller(motion);

    // Keyboard dismissal (Escape, Enter) moves focus without a pointer
    // event; hide the header then, unless the pointer still rests on it.
    {
        let (window, top, header) = (window.clone(), top.clone(), header.clone());
        window.clone().connect_focus_widget_notify(move |_| {
            if !window.is_fullscreen() || address_bar_in_use() {
                return;
            }
            let pointer_on_header = WidgetExt::display(&window)
                .default_seat()
                .and_then(|s| s.pointer())
                .and_then(|p| window.surface().and_then(|s| s.device_position(&p)))
                .is_some_and(|(_, y, _)| y <= header.height() as f64 + LEAVE_MARGIN);
            if !pointer_on_header {
                top.set_reveal_child(false);
            }
        });
    }
}

/// Ask the server to match the window, once the size has settled for 200 ms
/// so a drag-resize does not restart the encoder on every step. The
/// decoder's output zoom is requested on each poll that changes it, with
/// no settling delay: on the GPU tier the change costs only a reallocation
/// of the display images and a redraw, and the CPU tier picks it up with
/// its next frame.
fn install_resize_handler(ui: &Rc<App>) {
    // A Picture has no resize signal, so poll its allocation; the one-shot
    // timer sends only once the size has held for 200 ms.
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let scale = device_scale(&ui);
        ui.frame.set_scale(scale);
        request_zoom(&ui);
        let (w, h) = (
            ui.video.width() as f64 * scale,
            ui.video.height() as f64 * scale,
        );
        let size = (
            w.max(0.0).round() as u32 & !1,
            h.max(0.0).round() as u32 & !1,
        );
        let scale_changed = ui.view_scale.replace(scale) != scale;
        if size != ui.view_size.get() || scale_changed {
            ui.view_size.set(size);
            let ui = ui.clone();
            glib::timeout_add_local_once(Duration::from_millis(200), move || {
                if ui.view_size.get() == size {
                    request_resize(&ui);
                }
            });
        }
        glib::ControlFlow::Continue
    });
}

/// Ask the decoder for the integer zoom the current window allows, when it
/// differs from the last request. The view comes from the latest
/// StreamConfig, falling back to the last painted frame.
fn request_zoom(ui: &App) {
    let Some(s) = active(ui) else {
        return;
    };
    let mut view = s.server_view.get();
    if view == (0, 0) {
        view = s.stream_view.get();
    }
    let device = device_scale(ui);
    let (aw, ah) = (ui.video.width() as f64, ui.video.height() as f64);
    let want = paintable::layout(view, device, aw, ah).map_or(1, |l| l.zoom());
    if want == s.zoom.get() {
        return;
    }
    s.zoom.set(want);
    let tx = s.input_tx.borrow().clone();
    if let Some(tx) = tx {
        let _ = tx.send(clipboard::ToWorker::Zoom(want));
    }
}

/// Send a Resize if the server's view does not already match the window.
/// The gate compares the server's VIEW, not the stream: a stream the server
/// chose to send at reduced resolution is not a size mismatch, and asking
/// again would fight the server's own choice.
fn request_resize(ui: &App) {
    let Some(s) = active(ui) else {
        return;
    };
    let size = ui.view_size.get();
    let scale = device_scale(ui);
    if size.0 < 64
        || size.1 < 64
        || (s.resize_scale.get() == scale
            && (size == s.server_view.get() || size == s.resize_requested.get()))
    {
        return;
    }
    s.resize_requested.set(size);
    s.resize_scale.set(scale);
    send_to(
        &s,
        ClientMsg::Resize {
            width: size.0,
            height: size.1,
            scale: scale as f32,
        },
    );
}

/// GTK button number to evdev `BTN_*`.
fn evdev_button(n: u32) -> u32 {
    match n {
        1 => 0x110, // BTN_LEFT
        2 => 0x112, // BTN_MIDDLE
        3 => 0x111, // BTN_RIGHT
        8 => 0x116, // BTN_SIDE (back)
        9 => 0x115, // BTN_EXTRA (forward)
        _ => 0x110,
    }
}

fn device_scale(ui: &App) -> f64 {
    ui.window
        .surface()
        .map_or(ui.video.scale_factor() as f64, |s| s.scale())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn reconnect_invalidates_the_previous_scale_request() {
        let s = Session::new(Endpoint::Tcp("127.0.0.1:9070".into()));
        s.server_view.set((1920, 1080));
        s.resize_requested.set((1920, 1080));
        s.resize_scale.set(1.25);
        s.reset_resize();
        assert_ne!(s.resize_scale.get(), 1.25);
        assert_eq!(s.resize_requested.get(), (0, 0));
    }

    #[test]
    fn visible_shape_needs_alpha_and_contrast() {
        let transparent = [0u8; 16];
        let black = [0, 0, 0, 255].repeat(4);
        let mut arrow = [0u8; 16];
        arrow[3] = 255;
        assert!(!has_visible_shape(&transparent));
        assert!(!has_visible_shape(&black));
        assert!(has_visible_shape(&arrow));
    }

    #[test]
    fn parse_chord_and_double_and_none() {
        assert!(matches!(
            ReleaseHotkey::parse("none").unwrap(),
            ReleaseHotkey::None
        ));
        assert!(matches!(
            ReleaseHotkey::parse("").unwrap(),
            ReleaseHotkey::None
        ));

        match ReleaseHotkey::parse("shift+escape").unwrap() {
            ReleaseHotkey::Chord { mods, keyval } => {
                assert_eq!(mods, gdk::ModifierType::SHIFT_MASK);
                assert_eq!(keyval, gdk::Key::Escape);
            }
            _ => panic!("expected chord"),
        }

        match ReleaseHotkey::parse("Ctrl+Alt+q").unwrap() {
            ReleaseHotkey::Chord { mods, .. } => {
                assert!(mods.contains(gdk::ModifierType::CONTROL_MASK));
                assert!(mods.contains(gdk::ModifierType::ALT_MASK));
            }
            _ => panic!("expected chord"),
        }

        assert!(matches!(
            ReleaseHotkey::parse("double-escape").unwrap(),
            ReleaseHotkey::DoubleTap { .. }
        ));
        assert!(ReleaseHotkey::parse("ctrl+alt").is_err());
    }

    #[test]
    fn chord_matches_only_with_exact_mods() {
        let hk = ReleaseHotkey::parse("shift+escape").unwrap();
        let lt = RefCell::new(None);
        assert!(hk.matches(gdk::Key::Escape, gdk::ModifierType::SHIFT_MASK, &lt));
        // Bare Escape, no Shift: not the release hotkey.
        assert!(!hk.matches(gdk::Key::Escape, gdk::ModifierType::empty(), &lt));
        // Extra lock bits are ignored.
        assert!(hk.matches(
            gdk::Key::Escape,
            gdk::ModifierType::SHIFT_MASK | gdk::ModifierType::LOCK_MASK,
            &lt
        ));
    }

    #[test]
    fn double_tap_needs_two_quick_presses() {
        let hk = ReleaseHotkey::parse("double-escape").unwrap();
        let lt = RefCell::new(None);
        let none = gdk::ModifierType::empty();
        assert!(!hk.matches(gdk::Key::Escape, none, &lt)); // first tap is not the release trigger
        assert!(hk.matches(gdk::Key::Escape, none, &lt)); // quick second releases
        assert!(!hk.matches(gdk::Key::Escape, none, &lt)); // counter reset
    }
    #[test]
    fn macos_meta_and_super_both_match_the_command_release_hotkey() {
        let hk = ReleaseHotkey::parse("super+escape").unwrap();
        let last = RefCell::new(None);
        assert!(hk.matches(gdk::Key::Escape, gdk::ModifierType::META_MASK, &last));
        assert!(hk.matches(gdk::Key::Escape, gdk::ModifierType::SUPER_MASK, &last));
        assert!(!hk.matches(gdk::Key::Escape, gdk::ModifierType::ALT_MASK, &last));
    }

    #[test]
    fn repeated_escape_cannot_trigger_double_tap_release() {
        let hk = ReleaseHotkey::parse("double-escape").unwrap();
        let last = RefCell::new(None);
        let mut keys = keyboard::LocalKeys::default();
        let matches = |keys: &mut keyboard::LocalKeys| {
            keys.press(53) && hk.matches(gdk::Key::Escape, gdk::ModifierType::empty(), &last)
        };
        assert!(!matches(&mut keys));
        assert!(!matches(&mut keys));
        keys.release(53);
        assert!(matches(&mut keys));
    }
    #[test]
    fn first_and_repeated_clicks_queue_modifier_before_button_without_keyup() {
        for (index, code) in [(2, 42), (4, 29), (1, 125)] {
            let session = Session::new(Endpoint::Tcp("click".into()));
            let (tx, mut rx) = input_queue::channel();
            *session.input_tx.borrow_mut() = Some(tx);
            let mut modifiers = [false; 8];
            modifiers[index] = true;
            for _ in 0..2 {
                sync_keyboard_snapshot(&session, modifiers, false, keyboard::Policy::default());
                send_to(
                    &session,
                    ClientMsg::PointerButton {
                        button: 272,
                        pressed: true,
                    },
                );
            }
            assert!(
                matches!(rx.try_recv().unwrap(), clipboard::ToWorker::Send(ClientMsg::Key { keycode, pressed:true }) if keycode==code)
            );
            for _ in 0..2 {
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    clipboard::ToWorker::Send(ClientMsg::PointerButton { pressed: true, .. })
                ));
            }
            assert!(rx.try_recv().is_err());
        }
    }
    #[test]
    fn explicit_permission_request_is_exclusive_and_does_not_require_host() {
        assert!(Cli::try_parse_from(["gliff", "--request-capture-permissions"]).is_ok());
        assert!(Cli::try_parse_from([
            "gliff",
            "--request-capture-permissions",
            "--capture-system-shortcuts"
        ])
        .is_err());
        assert!(
            Cli::try_parse_from(["gliff", "--request-capture-permissions", "linux-host"]).is_err()
        );
    }
    #[test]
    fn key_cleanup_uses_the_original_sessions_sender_not_the_new_tabs() {
        let first = Session::new(Endpoint::Tcp("first".into()));
        let second = Session::new(Endpoint::Tcp("second".into()));
        let (tx1, mut rx1) = input_queue::channel();
        let (tx2, mut rx2) = input_queue::channel();
        *first.input_tx.borrow_mut() = Some(tx1);
        *second.input_tx.borrow_mut() = Some(tx2);
        first
            .keyboard
            .borrow_mut()
            .press(0, keyboard::Policy::default());
        release_session_keyboard(&first);
        assert!(matches!(
            rx1.try_recv().unwrap(),
            clipboard::ToWorker::Send(ClientMsg::Key {
                keycode: 30,
                pressed: false
            })
        ));
        assert!(rx2.try_recv().is_err());
        release_session_keyboard(&first);
        assert!(rx1.try_recv().is_err());
    }
    #[test]
    fn letter_release_hotkeys_use_the_unmodified_layout_key() {
        let last = RefCell::new(None);
        let shift = ReleaseHotkey::parse("shift+q").unwrap();
        assert!(shift.matches_event(
            gdk::Key::Q,
            Some(gdk::Key::q),
            gdk::ModifierType::SHIFT_MASK,
            &last
        ));
        assert!(!shift.matches_event(
            gdk::Key::Q,
            Some(gdk::Key::q),
            gdk::ModifierType::empty(),
            &last
        ));
    }

    #[test]
    fn option_transformed_letter_does_not_hide_the_release_trigger() {
        let hk = ReleaseHotkey::parse("ctrl+alt+q").unwrap();
        let last = RefCell::new(None);
        assert!(hk.matches_event(
            gdk::Key::oe,
            Some(gdk::Key::q),
            gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK,
            &last
        ));
        assert!(!hk.matches_event(
            gdk::Key::oe,
            Some(gdk::Key::q),
            gdk::ModifierType::ALT_MASK,
            &last
        ));
    }

    #[test]
    fn double_letter_tap_normalizes_the_key_without_counting_repeat() {
        let hk = ReleaseHotkey::parse("double-q").unwrap();
        let last = RefCell::new(None);
        let mut keys = keyboard::LocalKeys::default();
        let matches = |keys: &mut keyboard::LocalKeys| {
            keys.press(12)
                && hk.matches_event(
                    gdk::Key::Q,
                    Some(gdk::Key::q),
                    gdk::ModifierType::SHIFT_MASK,
                    &last,
                )
        };
        assert!(!matches(&mut keys));
        assert!(!matches(&mut keys));
        keys.release(12);
        assert!(matches(&mut keys));
    }

    #[test]
    fn release_hotkey_falls_back_to_reported_special_key_without_a_base() {
        let hk = ReleaseHotkey::parse("shift+escape").unwrap();
        let last = RefCell::new(None);
        assert!(hk.matches_event(gdk::Key::Escape, None, gdk::ModifierType::SHIFT_MASK, &last));
        assert!(hk.matches_event(
            gdk::Key::Escape,
            Some(gdk::Key::VoidSymbol),
            gdk::ModifierType::SHIFT_MASK,
            &last
        ));
    }
}
