use anyhow::{Result, bail};

#[cfg(unix)]
pub fn is_admin() -> bool {
	nix::unistd::Uid::effective().is_root()
}

#[cfg(windows)]
pub fn is_admin() -> bool {
	// SAFETY: IsUserAnAdmin is a thread-safe no-arg syscall.
	unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

pub fn require_admin() -> Result<()> {
	if !is_admin() {
		bail!("this operation must be run as root / Administrator");
	}
	Ok(())
}

/// Plain-English hint shown when binding a port below 1024 fails with EACCES.
/// Used by both the CLI start path and the GUI's error display.
#[cfg(unix)]
pub fn privileged_port_hint() -> String {
	let bin = std::env::current_exe()
		.ok()
		.as_deref()
		.map_or_else(|| "<binary>".into(), |p| p.display().to_string());
	format!(
		"Ports below 1024 need elevated privileges. Either:\n  \
		 - re-run with `sudo`, or\n  \
		 - grant the binary the bind capability once:\n      \
		 sudo setcap cap_net_bind_service=+ep {bin}\n    \
		 (re-apply after each rebuild; hosts-file edits still need root unless --no-redirect).",
	)
}

#[cfg(windows)]
pub fn privileged_port_hint() -> String {
	"Ports below 1024 need elevated privileges. Re-launch this binary from an \
	 elevated (Administrator) PowerShell or cmd."
		.into()
}

/// On Windows the binary is built as the `windows` subsystem so double-clicking
/// it doesn't briefly flash a console window. That also means CLI invocations
/// (`photobooth-bridge.exe start`) get no stdout/stderr by default — call this
/// at startup to attach to the parent's console if one exists, so tracing
/// output still reaches a developer running from PowerShell. No-op on Unix.
pub fn attach_parent_console() {
	#[cfg(windows)]
	{
		use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
		// SAFETY: no arguments to validate; returns 0 when there's no parent
		// console (the GUI double-click case), which is exactly what we want.
		unsafe {
			let _ = AttachConsole(ATTACH_PARENT_PROCESS);
		}
	}
}

/// Wait for a Ctrl-C / SIGTERM on the host OS, then resolve.
pub async fn wait_for_shutdown() {
	#[cfg(unix)]
	{
		use tokio::signal::unix::{SignalKind, signal};
		let mut sigint = signal(SignalKind::interrupt()).expect("install SIGINT");
		let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM");
		tokio::select! {
			_ = sigint.recv() => {}
			_ = sigterm.recv() => {}
		}
	}
	#[cfg(windows)]
	{
		// `ctrl_c` covers Ctrl-C and console close on Windows; we don't bother
		// with the rare WM_CLOSE / SIGTERM-equivalent paths.
		let _ = tokio::signal::ctrl_c().await;
	}
}
