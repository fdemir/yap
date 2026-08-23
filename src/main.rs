use std::{env, io, process::ExitCode};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use yap::{
    agent::{Agent, AgentError, AgentEvent},
    app::App,
    approval::{ChannelApprovalBroker, Decision},
    model::OpenAiModel,
    terminal::TerminalSession,
    tools::{ApplyPatchTool, ListFilesTool, ReadFileTool, RunCommandTool},
};

const DEFAULT_MODEL: &str = "gpt-5.3-codex";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

struct TurnRequest {
    prompt: String,
    cancellation: CancellationToken,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("yap: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let api_key = env::var("OPENAI_API_KEY").map_err(|_| "OPENAI_API_KEY must be set")?;
    let base_url = env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.into());
    let endpoint = format!("{}/responses", base_url.trim_end_matches('/'));
    let model_name = env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.into());
    let workspace = std::fs::canonicalize(env::current_dir()?)?;

    let model = OpenAiModel::new(endpoint, api_key);
    let mut agent = Agent::new(model, model_name.clone());
    agent.register_tool(ListFilesTool::new(&workspace)?);
    agent.register_tool(ReadFileTool::new(&workspace)?);
    agent.register_tool(ApplyPatchTool::new(&workspace)?);
    agent.register_tool(RunCommandTool::new(&workspace)?);

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<TurnRequest>(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(128);
    let (approval_tx, mut approval_rx) = mpsc::channel(8);
    agent.set_event_sender(event_tx.clone());
    agent.set_approval_broker(ChannelApprovalBroker::new(approval_tx));

    let error_events = event_tx.clone();
    let agent_task = tokio::spawn(async move {
        while let Some(request) = prompt_rx.recv().await {
            match agent
                .run_turn_with_cancellation(request.prompt, request.cancellation)
                .await
            {
                Ok(_) => {}
                Err(AgentError::Cancelled) => {
                    let _ = error_events.send(AgentEvent::TurnCancelled).await;
                }
                Err(error) => {
                    let _ = error_events
                        .send(AgentEvent::TurnFailed(error.to_string()))
                        .await;
                }
            }
        }
    });

    let workspace_label = workspace_label(&workspace);
    let mut terminal = TerminalSession::start()?;
    let result = run_tui(
        terminal.terminal_mut(),
        &model_name,
        &workspace_label,
        prompt_tx,
        &mut event_rx,
        &mut approval_rx,
    )
    .await;
    terminal.restore_now();
    agent_task.abort();
    let _ = agent_task.await;
    result?;
    Ok(())
}

fn workspace_label(workspace: &std::path::Path) -> String {
    let branch = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(workspace)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|branch| branch.trim().to_owned())
        .filter(|branch| !branch.is_empty());
    match branch {
        Some(branch) => format!("{} ({branch})", workspace.display()),
        None => workspace.display().to_string(),
    }
}

async fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    model: &str,
    workspace: &str,
    prompt_tx: mpsc::Sender<TurnRequest>,
    event_rx: &mut mpsc::Receiver<AgentEvent>,
    approval_rx: &mut mpsc::Receiver<yap::approval::PendingApproval>,
) -> io::Result<()> {
    let mut app = App::new();
    let mut terminal_events = EventStream::new();
    let mut active_cancellation: Option<CancellationToken> = None;
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        terminal.draw(|frame| yap::ui::render(frame, &mut app, model, workspace))?;
        tokio::select! {
            signal = &mut shutdown => {
                signal?;
                if let Some(cancellation) = &active_cancellation {
                    cancellation.cancel();
                }
                return Ok(());
            }
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        if !handle_key(&mut app, key, &prompt_tx, &mut active_cancellation).await {
                            return Ok(());
                        }
                    }
                    Some(Ok(Event::Paste(text))) if !app.has_pending_approval() => {
                        app.insert_text(&text);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => return Ok(()),
                }
            }
            Some(event) = event_rx.recv() => {
                if matches!(
                    &event,
                    AgentEvent::TurnFinished { .. }
                        | AgentEvent::TurnCancelled
                        | AgentEvent::TurnFailed(_)
                ) {
                    active_cancellation = None;
                }
                app.reduce(event);
            }
            Some(approval) = approval_rx.recv() => app.receive_approval(approval),
        }
    }
}

async fn handle_key(
    app: &mut App,
    key: crossterm::event::KeyEvent,
    prompt_tx: &mpsc::Sender<TurnRequest>,
    active_cancellation: &mut Option<CancellationToken>,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return false;
    }

    if app.has_pending_approval() {
        match key.code {
            KeyCode::Enter
                if !key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                app.decide(Decision::Allow);
            }
            KeyCode::Char('d') => app.decide(Decision::Deny),
            KeyCode::Esc => cancel_active_turn(app, active_cancellation),
            _ => {}
        }
        return true;
    }

    match key.code {
        KeyCode::Esc if app.status() == yap::app::Status::Working => {
            cancel_active_turn(app, active_cancellation);
        }
        KeyCode::Enter
            if key
                .modifiers
                .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
        {
            app.insert_newline();
        }
        KeyCode::Enter
            if matches!(
                app.status(),
                yap::app::Status::Ready | yap::app::Status::Failed
            ) =>
        {
            if let Some(prompt) = app.submit() {
                let cancellation = CancellationToken::new();
                *active_cancellation = Some(cancellation.clone());
                let _ = prompt_tx
                    .send(TurnRequest {
                        prompt,
                        cancellation,
                    })
                    .await;
            }
        }
        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.previous_prompt();
        }
        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.next_prompt();
        }
        KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.move_cursor_home();
        }
        KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.move_cursor_end();
        }
        KeyCode::Backspace => app.pop_input(),
        KeyCode::Delete => app.delete_input(),
        KeyCode::Left => app.move_cursor_left(),
        KeyCode::Right => app.move_cursor_right(),
        KeyCode::Home if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_to_top(),
        KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => app.scroll_to_bottom(),
        KeyCode::Home => app.move_cursor_home(),
        KeyCode::End => app.move_cursor_end(),
        KeyCode::Up => app.move_cursor_up(),
        KeyCode::Down => app.move_cursor_down(),
        KeyCode::PageUp => app.scroll_page_up(),
        KeyCode::PageDown => app.scroll_page_down(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.push_input(character);
        }
        _ => {}
    }
    true
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut terminate = signal(SignalKind::terminate())?;
        let mut hangup = signal(SignalKind::hangup())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
            _ = hangup.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

fn cancel_active_turn(app: &mut App, active_cancellation: &mut Option<CancellationToken>) {
    app.cancel_active_turn();
    if let Some(cancellation) = active_cancellation {
        cancellation.cancel();
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEvent;

    use super::*;

    #[tokio::test]
    async fn modified_enter_inserts_a_newline_and_plain_enter_submits() {
        let mut app = App::new();
        let (prompt_tx, mut prompt_rx) = mpsc::channel(1);
        let mut cancellation = None;

        for key in [
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            assert!(handle_key(&mut app, key, &prompt_tx, &mut cancellation).await);
        }

        let request = prompt_rx.recv().await.expect("prompt should be submitted");
        assert_eq!(request.prompt, "a\nb");
        assert!(cancellation.is_some());
    }
}
