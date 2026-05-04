#[cfg(target_os = "macos")]
mod platform;

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    match std::panic::catch_unwind(platform::run) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("WRY helper panicked during startup");
            println!(
                "{}",
                serde_json::json!({
                    "ok": false,
                    "error": message,
                })
            );
            anyhow::bail!("{message}");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("codex-agent-browser-wry is not supported on this platform yet")
}
