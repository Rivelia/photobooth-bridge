fn main() {
	let target = std::env::var("TARGET").unwrap_or_default();
	if target.contains("windows") {
		// Embeds the UAC manifest so double-clicking the .exe triggers a UAC
		// prompt and the process inherits Administrator — required to bind 443
		// and edit the system hosts file. Works for both MSVC (rc.exe) and
		// MinGW (windres) toolchains via the embed-resource crate.
		let _ = embed_resource::compile("app.rc", embed_resource::NONE);
	}
}
