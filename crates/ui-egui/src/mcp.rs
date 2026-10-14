//! Tools ▸ MCP Server: lets AI agents drive the running app. The host starts the loopback
//! control channel ([`crate::Services::start_control`]); MCP clients reach it through
//! `cadcraft-cli mcp --connect 127.0.0.1:PORT`, which speaks MCP on stdio and forwards to it.
//!
//! `ui.mcp` with no parameters opens the dialog; with `{on, port, atLaunch}` it switches the
//! server, picks its port and whether it starts when CADCraft opens (all optional), and answers
//! the state.

use std::sync::mpsc::Receiver;

use serde_json::{Value, json};

use crate::CadApp;
use crate::control::ControlRequest;

/// The port suggested until the user picks another (the one the docs use).
pub const DEFAULT_PORT: u16 = 7979;

/// A running control channel, as the host started it.
pub struct ControlServer {
    /// Requests from clients, answered by the app each frame.
    pub requests: Receiver<ControlRequest>,
    /// The port it listens on (the one asked for, or the system's choice for `0`).
    pub port: u16,
    /// Closes the listener.
    pub stop: Box<dyn FnOnce()>,
}

/// Starts the control channel on a loopback port; the desktop host supplies it, the web host
/// can't listen.
pub type StartControl = Box<dyn Fn(u16) -> Result<ControlServer, String>>;

/// The server's settings and state.
pub struct State {
    /// The port to listen on.
    pub port: u16,
    /// Start the server when CADCraft opens.
    pub at_launch: bool,
    /// The running server: its port and how to stop it.
    running: Option<(u16, Box<dyn FnOnce()>)>,
    /// Why the last start failed.
    pub error: Option<String>,
    /// Bumped whenever the server starts or stops, so a request that switched it while the app
    /// was answering requests isn't followed by the old receiver ([`CadApp::drain_control`]).
    pub(crate) generation: u64,
}

impl Default for State {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, at_launch: false, running: None, error: None, generation: 0 }
    }
}

impl State {
    /// The port the server listens on, if it is running.
    pub fn running_port(&self) -> Option<u16> {
        self.running.as_ref().map(|(port, _)| *port)
    }

    /// The port to start on when CADCraft opens, if the user asked for that.
    pub fn launch_port(&self) -> Option<u16> {
        self.at_launch.then_some(self.port)
    }

    /// Settings kept across restarts; `None` when they are the defaults.
    pub fn prefs(&self) -> Option<Value> {
        (self.port != DEFAULT_PORT || self.at_launch).then(|| json!({"port": self.port, "atLaunch": self.at_launch}))
    }

    pub fn load_prefs(&mut self, v: &Value) {
        if let Some(port) = v.get("port").and_then(port_from) {
            self.port = port;
        }
        if let Some(on) = v.get("atLaunch").and_then(Value::as_bool) {
            self.at_launch = on;
        }
    }
}

/// A port number from JSON: 1–65535.
fn port_from(v: &Value) -> Option<u16> {
    v.as_u64().and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0)
}

/// The command an MCP client runs: `cadcraft-cli` next to this app when it is there, else the
/// one on the `PATH`.
pub fn cli_path() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let name = format!("cadcraft-cli{}", std::env::consts::EXE_SUFFIX);
        if let Some(cli) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|dir| dir.join(&name))).filter(|p| p.is_file()) {
            return cli.to_string_lossy().into_owned();
        }
    }
    "cadcraft-cli".into()
}

/// The arguments an MCP client passes to [`cli_path`] for the app on `port`. `--lazy`: the client
/// may start before the server is switched on.
pub fn cli_args(port: u16) -> Vec<String> {
    vec!["mcp".into(), "--connect".into(), format!("127.0.0.1:{port}"), "--lazy".into()]
}

/// The `mcpServers` entry most MCP clients (Claude Desktop, Cursor, …) take in their settings.
pub fn client_config(port: u16) -> String {
    let config = json!({"mcpServers": {"cadcraft": {"command": cli_path(), "args": cli_args(port)}}});
    serde_json::to_string_pretty(&config).unwrap_or_default()
}

/// The Claude Code command that adds the server.
pub fn claude_code_command(port: u16) -> String {
    let quote = |s: &str| if s.contains([' ', '\'', '"']) { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() };
    format!("claude mcp add cadcraft -- {} {}", quote(&cli_path()), cli_args(port).join(" "))
}

impl CadApp {
    /// Start the control channel on `port` (stopping one already running); the port it listens
    /// on, or why it couldn't start. The saved port and launch choice are left alone.
    pub fn start_control(&mut self, port: u16) -> Result<u16, String> {
        self.stop_control();
        let Some(start) = self.services.start_control.as_ref() else {
            return Err("The MCP server needs the desktop app".into());
        };
        match start(port) {
            Ok(server) => {
                self.control_rx = Some(server.requests);
                self.mcp.running = Some((server.port, server.stop));
                self.mcp.error = None;
                self.mcp.generation += 1;
                Ok(server.port)
            }
            Err(e) => {
                self.mcp.error = Some(e.clone());
                Err(e)
            }
        }
    }

    /// Stop the control channel, if it is running.
    pub fn stop_control(&mut self) {
        let running = self.mcp.running.take();
        let had_requests = self.control_rx.take().is_some();
        if let Some((_, stop)) = running {
            stop();
            self.mcp.generation += 1;
        } else if had_requests {
            self.mcp.generation += 1;
        }
    }

    /// Whether the control channel is answering requests (also when it was handed in with
    /// [`CadApp::with_control`]).
    pub fn control_running(&self) -> bool {
        self.mcp.running.is_some() || self.control_rx.is_some()
    }
}

/// `ui.mcp`.
pub fn route(app: &mut CadApp, id: &str, params: &Value) -> Option<Result<Value, String>> {
    if id != "ui.mcp" {
        return None;
    }
    if params.is_null() {
        app.ui.dialog = Some("mcp".into());
        return Some(Ok(summary(app)));
    }
    Some(apply(app, params).map(|()| summary(app)))
}

fn apply(app: &mut CadApp, params: &Value) -> Result<(), String> {
    let port = match params.get("port") {
        None => None,
        Some(v) => Some(port_from(v).ok_or("port must be a whole number from 1 to 65535")?),
    };
    let on = match params.get("on") {
        None => None,
        Some(v) => Some(v.as_bool().ok_or("on must be true or false")?),
    };
    let at_launch = match params.get("atLaunch") {
        None => None,
        Some(v) => Some(v.as_bool().ok_or("atLaunch must be true or false")?),
    };
    if let Some(port) = port {
        app.mcp.port = port;
    }
    if let Some(at_launch) = at_launch {
        app.mcp.at_launch = at_launch;
    }
    match on {
        Some(true) if app.mcp.running_port() != Some(app.mcp.port) => app.start_control(app.mcp.port).map(|_| ()),
        Some(false) => {
            app.stop_control();
            Ok(())
        }
        _ => Ok(()),
    }
}

fn summary(app: &CadApp) -> Value {
    let port = app.mcp.running_port().unwrap_or(app.mcp.port);
    json!({
        "running": app.control_running(),
        "port": app.mcp.running_port(),
        "preferredPort": app.mcp.port,
        "atLaunch": app.mcp.at_launch,
        "available": app.services.start_control.is_some(),
        "error": app.mcp.error,
        "command": cli_path(),
        "args": cli_args(port),
    })
}

pub fn dialog(app: &mut CadApp, ctx: &egui::Context, open: &mut bool) {
    egui::Window::new(crate::tl!("MCP Server")).id(egui::Id::new("mcp")).open(open).resizable(false).show(ctx, |ui| {
        ui.set_max_width(460.0);
        ui.label(crate::tl!("Lets AI agents (Claude and other MCP clients) draw, edit and inspect drawings in CADCraft."));
        if app.services.start_control.is_none() {
            ui.separator();
            ui.label(crate::tl!("The MCP server needs the desktop app."));
            return;
        }
        ui.separator();
        let mut on = app.control_running();
        if ui.checkbox(&mut on, crate::tl!("Enable MCP server")).changed() {
            let _ = app.run("ui.mcp", json!({ "on": on }));
        }
        ui.horizontal(|ui| {
            ui.label(crate::tl!("Port"));
            // While running: the port it listens on (`--control` may have picked another).
            let running = app.mcp.running_port();
            let mut port = running.unwrap_or(app.mcp.port);
            if ui.add_enabled(running.is_none(), egui::DragValue::new(&mut port).range(1..=65535)).changed() {
                let _ = app.run("ui.mcp", json!({ "port": port }));
            }
        });
        let mut at_launch = app.mcp.at_launch;
        if ui.checkbox(&mut at_launch, crate::tl!("Start when CADCraft opens")).changed() {
            let _ = app.run("ui.mcp", json!({ "atLaunch": at_launch }));
        }
        match (app.mcp.running_port(), &app.mcp.error) {
            (Some(port), _) => {
                ui.label(crate::tf!("Listening on 127.0.0.1:{port}", port = port));
            }
            (None, _) if app.control_running() => {
                ui.label(crate::tl!("Running (started with --control)"));
            }
            (None, Some(error)) => {
                ui.colored_label(ui.visuals().error_fg_color, error.as_str());
            }
            (None, None) => {
                ui.label(crate::tl!("Off"));
            }
        }
        ui.label(crate::tl!("While it is on, any program on this computer can control CADCraft."));
        ui.separator();
        let port = app.mcp.running_port().unwrap_or(app.mcp.port);
        ui.strong(crate::tl!("Connect an MCP client"));
        ui.label(crate::tl!("Add this server to the client's MCP settings:"));
        snippet(ui, "mcp-config", &client_config(port));
        ui.label(crate::tl!("Claude Code:"));
        snippet(ui, "mcp-claude-code", &claude_code_command(port));
    });
}

/// Read-only monospace text with a Copy button.
fn snippet(ui: &mut egui::Ui, id: &str, text: &str) {
    ui.horizontal(|ui| {
        let mut shown = text.to_string();
        ui.add(egui::TextEdit::multiline(&mut shown).id_salt(id).code_editor().desired_rows(1).desired_width(380.0).interactive(true));
        if ui.button(crate::tl!("Copy")).clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use cadcraft_engine::Session;
    use serde_json::{Value, json};

    use super::*;
    use crate::Services;

    /// An app whose host "listens" without a socket; `log` records starts and stops.
    fn app(log: &Rc<RefCell<Vec<String>>>) -> CadApp {
        let log = Rc::clone(log);
        let start: StartControl = Box::new(move |port| {
            if port == 1 {
                return Err("port 1 is taken".into());
            }
            let port = if port == 0 { 50000 } else { port };
            log.borrow_mut().push(format!("start {port}"));
            let log = Rc::clone(&log);
            let (_tx, requests) = std::sync::mpsc::channel();
            Ok(ControlServer { requests, port, stop: Box::new(move || log.borrow_mut().push(format!("stop {port}"))) })
        });
        CadApp::new(Session::new(), Services { start_control: Some(start), ..Services::default() })
    }

    #[test]
    fn the_menu_opens_the_dialog_and_json_switches_the_server() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = app(&log);
        assert!(crate::menus::is_ui_command("ui.mcp"));
        app.run("ui.mcp", Value::Null).unwrap();
        assert_eq!(app.ui.dialog.as_deref(), Some("mcp"));
        assert!(!app.control_running(), "opening the dialog starts nothing");

        let r = app.run("ui.mcp", json!({"on": true})).unwrap();
        assert_eq!((r["running"].as_bool(), r["port"].as_u64()), (Some(true), Some(u64::from(DEFAULT_PORT))));
        assert_eq!(r["args"], json!(["mcp", "--connect", "127.0.0.1:7979", "--lazy"]));
        // Already on: nothing restarts.
        app.run("ui.mcp", json!({"on": true})).unwrap();
        // A new port while on moves the server.
        app.run("ui.mcp", json!({"on": true, "port": 8123})).unwrap();
        assert_eq!(app.mcp.running_port(), Some(8123));
        let r = app.run("ui.mcp", json!({"on": false})).unwrap();
        assert_eq!(r["running"], json!(false));
        assert_eq!(*log.borrow(), ["start 7979", "stop 7979", "start 8123", "stop 8123"]);
    }

    #[test]
    fn a_failed_start_is_reported_and_bad_params_are_refused() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = app(&log);
        let e = app.run("ui.mcp", json!({"on": true, "port": 1})).unwrap_err();
        assert!(e.contains("taken"), "{e}");
        assert_eq!(app.mcp.error.as_deref(), Some("port 1 is taken"));
        for bad in
            [json!({"port": 0}), json!({"port": 70000}), json!({"port": -5}), json!({"port": "x"}), json!({"on": "yes"}), json!({"atLaunch": 1})]
        {
            assert!(app.run("ui.mcp", bad.clone()).is_err(), "{bad}");
        }
        assert!(log.borrow().is_empty());
        assert!(!app.control_running());
    }

    #[test]
    fn without_a_host_listener_the_server_is_unavailable() {
        let mut app = CadApp::new(Session::new(), Services::default());
        let e = app.run("ui.mcp", json!({"on": true})).unwrap_err();
        assert!(e.contains("desktop"), "{e}");
        assert_eq!(app.run("ui.mcp", json!({})).unwrap()["available"], json!(false));
    }

    #[test]
    fn port_and_launch_choice_are_kept_across_restarts() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut app = app(&log);
        assert!(app.mcp.prefs().is_none(), "defaults aren't saved");
        app.run("ui.mcp", json!({"port": 9001, "atLaunch": true})).unwrap();
        assert!(!app.control_running(), "choosing settings starts nothing");
        let prefs = app.prefs_json();
        let mut again = self::app(&log);
        again.load_prefs(&prefs);
        assert_eq!((again.mcp.port, again.mcp.launch_port()), (9001, Some(9001)));
        // Hostile saved values are ignored.
        again.mcp.load_prefs(&json!({"port": 1e12, "atLaunch": "x"}));
        assert_eq!((again.mcp.port, again.mcp.at_launch), (9001, true));
    }

    #[test]
    fn client_snippets_name_the_port() {
        assert!(client_config(8123).contains("127.0.0.1:8123"));
        assert!(claude_code_command(8123).starts_with("claude mcp add cadcraft -- "));
        assert!(claude_code_command(8123).ends_with("mcp --connect 127.0.0.1:8123 --lazy"));
    }
}
