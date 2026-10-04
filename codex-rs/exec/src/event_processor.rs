use std::io;
use std::io::Write;
use std::path::Path;

use codex_app_server_protocol::ServerNotification;
use codex_core::config::Config;
use codex_protocol::protocol::SessionConfiguredEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexStatus {
    Running,
    InitiateShutdown,
}

pub(crate) trait EventProcessor {
    /// Print summary of effective configuration and user prompt.
    fn print_config_summary(
        &mut self,
        config: &Config,
        prompt: &str,
        session_configured: &SessionConfiguredEvent,
    );

    /// Handle a single typed app-server notification emitted by the agent.
    fn process_server_notification(&mut self, notification: ServerNotification) -> CodexStatus;

    /// Handle a local exec warning that is not represented as an app-server notification.
    fn process_warning(&mut self, message: String) -> CodexStatus;

    fn print_final_output(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn handle_last_message(
    last_agent_message: Option<&str>,
    output_file: &Path,
) -> io::Result<()> {
    let message = last_agent_message.unwrap_or_default();
    write_last_message_file(message, Some(output_file))?;
    if last_agent_message.is_none() {
        writeln!(
            io::stderr().lock(),
            "Warning: no last agent message; wrote empty content to {}",
            output_file.display()
        )?;
    }
    Ok(())
}

fn write_last_message_file(contents: &str, last_message_path: Option<&Path>) -> io::Result<()> {
    if let Some(path) = last_message_path {
        std::fs::write(path, contents).map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "failed to write last message file {}: {err}",
                    path.display()
                ),
            )
        })?;
    }
    Ok(())
}
