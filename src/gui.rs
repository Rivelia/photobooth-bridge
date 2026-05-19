//! Single-window GUI for the bridge — what double-clicking the .exe launches.
//!
//! Two threads cooperate via a shared mutex + an unbounded command channel:
//!
//! - **Main / eframe thread.** Renders the egui UI, sends commands
//!   ([`Cmd`]) when the user clicks a button. Never blocks on I/O.
//! - **Tokio runtime thread.** Owns the [`ProxyHandle`] when the bridge is
//!   running, runs the privileged work (CA install, hosts-file edits, listener
//!   bind), and writes state transitions back into [`Shared`].
//!
//! Each write to `Shared` calls [`egui::Context::request_repaint`] so the UI
//! reflects state changes immediately instead of waiting for the next mouse
//! move.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use eframe::egui;
use tokio::sync::mpsc;

use crate::{install, lifecycle, platform};

const QR_TOOL_URL: &str = "https://naraka.wiki/photo-booth";

/// Coarse-grained app state. Driven by the runtime thread; the UI is a pure
/// function of this enum + [`Shared::last_error`].
#[derive(Clone, Copy, PartialEq)]
pub enum State {
	/// No CA on disk yet — first-run welcome view.
	NeedsCa,
	Installing,
	/// CA installed, proxy not running.
	Stopped,
	Starting,
	Running,
	Stopping,
	Uninstalling,
}

impl State {
	fn initial() -> Self {
		if install::ca_files_exist() { Self::Stopped } else { Self::NeedsCa }
	}
	/// True when the proxy isn't running and isn't mid-transition — safe to
	/// close the window or fire `Uninstall` without racing the runtime thread.
	fn is_idle(&self) -> bool {
		matches!(self, Self::Stopped | Self::NeedsCa)
	}
}

/// Shared state read by the UI and written by the runtime thread.
struct Shared {
	state: State,
	last_error: Option<String>,
	/// Set by the eframe `creation_context` so background-thread updates can
	/// kick the UI to repaint.
	ctx: Option<egui::Context>,
}

impl Shared {
	/// Transition to `state` and clear any previous error.
	fn set_ok(&mut self, state: State) {
		self.state = state;
		self.last_error = None;
		self.repaint();
	}
	/// Transition to `state` and surface `msg` as the current error.
	fn set_err(&mut self, state: State, msg: String) {
		tracing::warn!("gui error: {msg}");
		self.state = state;
		self.last_error = Some(msg);
		self.repaint();
	}
	/// Transition to `state` without touching the error field (used for
	/// transient in-progress states where any previous error should keep
	/// showing until the next terminal outcome).
	fn set_transient(&mut self, state: State) {
		self.state = state;
		self.repaint();
	}
	fn repaint(&self) {
		if let Some(ctx) = &self.ctx {
			ctx.request_repaint();
		}
	}
}

enum Cmd {
	InstallAndStart,
	Start,
	Stop,
	Uninstall,
	Quit,
}

pub fn run() -> Result<()> {
	if !platform::is_admin() {
		// On Windows the embedded UAC manifest auto-elevates, so we shouldn't
		// ever land here in production. On Linux the GUI still needs root for
		// 443 + hosts-file edits — surface that clearly instead of failing
		// halfway through Start.
		anyhow::bail!(
			"this program needs Administrator (Windows) or root (Linux) to bind \
			 443 and edit the system hosts file.\n\nRe-launch from an elevated \
			 shell, or on Linux run with `sudo`."
		);
	}

	let shared = Arc::new(Mutex::new(Shared {
		state: State::initial(),
		last_error: None,
		ctx: None,
	}));
	let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<Cmd>();

	let shared_rt = Arc::clone(&shared);
	let runtime_thread = std::thread::Builder::new()
		.name("proxy-runtime".into())
		.spawn(move || {
			let rt = tokio::runtime::Builder::new_multi_thread()
				.enable_all()
				.build()
				.expect("build tokio runtime");
			rt.block_on(runtime_loop(cmd_rx, shared_rt));
		})?;

	let app = App {
		shared: Arc::clone(&shared),
		cmd_tx: cmd_tx.clone(),
		pending_close: false,
	};

	let native_options = eframe::NativeOptions {
		viewport: egui::ViewportBuilder::default()
			.with_inner_size([560.0, 380.0])
			.with_min_inner_size([460.0, 320.0])
			.with_title("Photo Booth Bridge"),
		..Default::default()
	};

	let shared_for_init = Arc::clone(&shared);
	let run_res = eframe::run_native(
		"Photo Booth Bridge",
		native_options,
		Box::new(move |cc| {
			shared_for_init.lock().unwrap().ctx = Some(cc.egui_ctx.clone());
			Ok(Box::new(app))
		}),
	);

	// Window closed (or eframe errored out) — tell the runtime to clean up
	// and wait for it. ProxyHandle::shutdown runs the hosts-file scrub, so
	// the process intentionally doesn't exit until that completes.
	let _ = cmd_tx.send(Cmd::Quit);
	let _ = runtime_thread.join();

	run_res.map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
	Ok(())
}

async fn runtime_loop(mut cmd_rx: mpsc::UnboundedReceiver<Cmd>, shared: Arc<Mutex<Shared>>) {
	let mut proxy: Option<lifecycle::ProxyHandle> = None;

	while let Some(cmd) = cmd_rx.recv().await {
		match cmd {
			Cmd::InstallAndStart => {
				shared.lock().unwrap().set_transient(State::Installing);
				let install_res = tokio::task::spawn_blocking(|| {
					install::install_ca(install::NARAKA_STEAM_APP_ID, false)
				})
				.await;
				match install_res {
					Ok(Ok(_ca)) => {
						// start_proxy clears the error on its terminal Running
						// transition; nothing to do here.
						start_proxy(&shared, &mut proxy).await;
					}
					Ok(Err(err)) => {
						shared.lock().unwrap().set_err(
							State::NeedsCa,
							format!("Couldn't install certificate: {err:#}"),
						);
					}
					Err(join_err) => {
						shared.lock().unwrap().set_err(
							State::NeedsCa,
							format!("Install task panicked: {join_err}"),
						);
					}
				}
			}
			Cmd::Start => {
				if proxy.is_some() {
					continue;
				}
				start_proxy(&shared, &mut proxy).await;
			}
			Cmd::Stop => {
				stop_proxy(&shared, &mut proxy).await;
			}
			Cmd::Uninstall => {
				stop_proxy(&shared, &mut proxy).await;
				shared.lock().unwrap().set_transient(State::Uninstalling);
				let res = tokio::task::spawn_blocking(|| {
					install::uninstall(install::NARAKA_STEAM_APP_ID, false, false)
				})
				.await;
				let error_msg = match res {
					Ok(Ok(report)) if report.failures.is_empty() => None,
					Ok(Ok(report)) => Some(
						report
							.failures
							.iter()
							.map(|(l, e)| format!("{l}: {e:#}"))
							.collect::<Vec<_>>()
							.join("\n"),
					),
					Ok(Err(err)) => Some(format!("Uninstall failed: {err:#}")),
					Err(join_err) => Some(format!("Uninstall task panicked: {join_err}")),
				};
				let next = State::initial();
				let mut s = shared.lock().unwrap();
				match error_msg {
					Some(msg) => s.set_err(next, msg),
					None => s.set_ok(next),
				}
			}
			Cmd::Quit => {
				stop_proxy(&shared, &mut proxy).await;
				break;
			}
		}
	}
}

async fn start_proxy(shared: &Arc<Mutex<Shared>>, slot: &mut Option<lifecycle::ProxyHandle>) {
	shared.lock().unwrap().set_transient(State::Starting);
	match lifecycle::start(lifecycle::DEFAULT_PORT, false).await {
		Ok(handle) => {
			*slot = Some(handle);
			shared.lock().unwrap().set_ok(State::Running);
		}
		Err(err) => {
			shared.lock().unwrap().set_err(
				State::Stopped,
				format!("Couldn't start the bridge: {err:#}"),
			);
		}
	}
}

async fn stop_proxy(shared: &Arc<Mutex<Shared>>, slot: &mut Option<lifecycle::ProxyHandle>) {
	let Some(handle) = slot.take() else { return };
	shared.lock().unwrap().set_transient(State::Stopping);
	match handle.shutdown().await {
		Ok(()) => shared.lock().unwrap().set_ok(State::Stopped),
		Err(err) => shared.lock().unwrap().set_err(
			State::Stopped,
			format!("Shutdown error: {err:#}"),
		),
	}
}

struct App {
	shared: Arc<Mutex<Shared>>,
	cmd_tx: mpsc::UnboundedSender<Cmd>,
	/// User clicked the window close button while the proxy was still running
	/// (or mid-transition). We veto the close, drive the proxy to a stopped
	/// state, then re-emit Close so the hosts-file scrub completes before the
	/// window vanishes.
	pending_close: bool,
}

impl eframe::App for App {
	fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
		let ctx = ui.ctx().clone();

		// When the user clicks X while the bridge is running (or mid-transition),
		// we'd otherwise return from eframe immediately and only THEN process the
		// hosts-file scrub on the runtime thread — leaving the window vanished
		// while the system still has our `127.0.0.1 api.narakathegame.com` lines.
		// Instead, veto the close, kick off Stop, and re-emit Close once the
		// state machine settles.
		let (state_snapshot, last_error) = {
			let s = self.shared.lock().unwrap();
			(s.state, s.last_error.clone())
		};
		let safe_to_close = state_snapshot.is_idle();

		if ctx.input(|i| i.viewport().close_requested()) && !safe_to_close {
			self.pending_close = true;
			ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
		}
		if self.pending_close {
			if safe_to_close {
				self.pending_close = false;
				ctx.send_viewport_cmd(egui::ViewportCommand::Close);
			} else {
				// Drive the proxy to Stopped. If we're mid-transition (Starting,
				// Installing, etc.) we'll re-evaluate on the next repaint once
				// the state machine settles on Running, then send Stop then.
				if matches!(state_snapshot, State::Running) {
					self.send(Cmd::Stop);
				}
				ctx.request_repaint_after(Duration::from_millis(100));
			}
		}

		egui::Frame::central_panel(ui.style()).show(ui, |ui| {
			ui.add_space(8.0);
			ui.vertical_centered(|ui| {
				ui.heading("Photo Booth Bridge");
				ui.label(
					egui::RichText::new("Naraka: Bladepoint — cross-region QR import")
						.weak(),
				);
			});
			ui.add_space(12.0);
			ui.separator();
			ui.add_space(12.0);

			self.render_status(ui, &state_snapshot);
			ui.add_space(16.0);
			self.render_primary_action(ui, &state_snapshot);
			ui.add_space(20.0);
			self.render_converter_hint(ui);
			ui.add_space(8.0);

			if let Some(err) = last_error {
				ui.add_space(8.0);
				egui::Frame::group(ui.style())
					.fill(egui::Color32::from_rgb(80, 25, 25))
					.show(ui, |ui| {
						ui.colored_label(
							egui::Color32::from_rgb(255, 200, 200),
							format!("⚠ {err}"),
						);
					});
			}

			// Bottom-aligned uninstall button.
			ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
				ui.add_space(4.0);
				ui.horizontal(|ui| {
					ui.add_space(4.0);
					let removable = state_snapshot.is_idle();
					ui.add_enabled_ui(removable, |ui| {
						if ui
							.small_button("Remove certificate and clean up")
							.on_hover_text(
								"Removes the local certificate from your trust store, \
								 scrubs the hosts file, and deletes the CA files.",
							)
							.clicked()
						{
							self.send(Cmd::Uninstall);
						}
					});
				});
			});
		});
	}
}

impl App {
	fn send(&self, cmd: Cmd) {
		let _ = self.cmd_tx.send(cmd);
	}

	fn render_status(&self, ui: &mut egui::Ui, state: &State) {
		let (color, text) = match state {
			State::NeedsCa => (
				egui::Color32::from_rgb(180, 180, 180),
				"Not set up yet",
			),
			State::Installing => (egui::Color32::YELLOW, "Installing certificate…"),
			State::Stopped => (egui::Color32::from_rgb(180, 180, 180), "Bridge is off"),
			State::Starting => (egui::Color32::YELLOW, "Starting bridge…"),
			State::Running => (
				egui::Color32::from_rgb(80, 200, 120),
				"Bridge is ON — Naraka cross-region lookups will work",
			),
			State::Stopping => (egui::Color32::YELLOW, "Stopping bridge…"),
			State::Uninstalling => (egui::Color32::YELLOW, "Cleaning up…"),
		};
		ui.horizontal(|ui| {
			ui.add_space(8.0);
			ui.colored_label(color, "●");
			ui.label(egui::RichText::new(text).size(15.0));
		});
	}

	fn render_primary_action(&self, ui: &mut egui::Ui, state: &State) {
		ui.vertical_centered(|ui| match state {
			State::NeedsCa => {
				ui.label(
					"On first run, the bridge installs a local certificate so Naraka \
					 trusts the intercepted connection. The certificate only signs \
					 traffic while the bridge is running, and only on this machine.",
				);
				ui.add_space(8.0);
				if big_button(ui, "Install certificate and start bridge").clicked() {
					self.send(Cmd::InstallAndStart);
				}
			}
			State::Stopped => {
				if big_button(ui, "Start bridge").clicked() {
					self.send(Cmd::Start);
				}
			}
			State::Running => {
				if big_button(ui, "Stop bridge").clicked() {
					self.send(Cmd::Stop);
				}
			}
			State::Installing
			| State::Starting
			| State::Stopping
			| State::Uninstalling => {
				ui.add(egui::Spinner::new().size(28.0));
			}
		});
	}

	fn render_converter_hint(&self, ui: &mut egui::Ui) {
		ui.vertical_centered(|ui| {
			ui.horizontal(|ui| {
				ui.spacing_mut().item_spacing.x = 4.0;
				ui.label("Need a cross-region QR? Upload the foreign QR image to the");
				ui.hyperlink_to("converter", QR_TOOL_URL);
				ui.label("and scan the result in-game.");
			});
		});
	}
}

fn big_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
	ui.add_sized(
		[280.0, 36.0],
		egui::Button::new(egui::RichText::new(text).size(15.0)),
	)
}
