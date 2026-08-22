use std::{env, io, process::ExitCode};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use yap::{
    agent::{Agent, AgentEvent},
    app::App,
    approval::{ChannelApprovalBroker, Decision},
    model::OpenAiModel,
    tools::{ApplyPatchTool, ListFilesTool, ReadFileTool, RunCommandTool},
};

const DEFAULT_MODEL: &str = "gpt-5.3-codex";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

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

    let (prompt_tx, mut prompt_rx) = mpsc::channel::<String>(8);
    let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(128);
    let (approval_tx, mut approval_rx) = mpsc::channel(8);
    agent.set_event_sender(event_tx.clone());
    agent.set_approval_broker(ChannelApprovalBroker::new(approval_tx));

    let error_events = event_tx.clone();
    let agent_task = tokio::spawn(async move {
        while let Some(prompt) = prompt_rx.recv().await {
            if let Err(error) = agent.run_turn(prompt).await {
                let _ = error_events
                    .send(AgentEvent::TurnFailed(error.to_string()))
                    .await;
            }
        }
    });

    let workspace_label = workspace_label(&workspace);
    let mut terminal = ratatui::init();
    let result = run_tui(
        &mut terminal,
        &model_name,
        &workspace_label,
        prompt_tx,
        &mut event_rx,
        &mut approval_rx,
    )
    .await;
    ratatui::restore();
    agent_task.abort();
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
    prompt_tx: mpsc::Sender<String>,
    event_rx: &mut mpsc::Receiver<AgentEvent>,
    approval_rx: &mut mpsc::Receiver<yap::approval::PendingApproval>,
) -> io::Result<()> {
    let mut app = App::new();
    let mut terminal_events = EventStream::new();

    loop {
        terminal.draw(|frame| yap::ui::render(frame, &app, model, workspace))?;
        tokio::select! {
            terminal_event = terminal_events.next() => {
                match terminal_event {
                    Some(Ok(Event::Key(key))) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                        if !handle_key(&mut app, key, &prompt_tx).await {
                            return Ok(());
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return Err(error),
                    None => return Ok(()),
                }
            }
            Some(event) = event_rx.recv() => app.reduce(event),
            Some(approval) = approval_rx.recv() => app.receive_approval(approval),
        }
    }
}

async fn handle_key(
    app: &mut App,
    key: crossterm::event::KeyEvent,
    prompt_tx: &mpsc::Sender<String>,
) -> bool {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return false;
    }

    if app.has_pending_approval() {
        match key.code {
            KeyCode::Enter => app.decide(Decision::Allow),
            KeyCode::Char('d') | KeyCode::Esc => app.decide(Decision::Deny),
            _ => {}
        }
        return true;
    }

    match key.code {
        KeyCode::Enter => {
            if let Some(prompt) = app.submit() {
                let _ = prompt_tx.send(prompt).await;
            }
        }
        KeyCode::Backspace => app.pop_input(),
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            app.push_input(character);
        }
        KeyCode::Up => app.scroll_up(),
        KeyCode::Down => app.scroll_down(),
        _ => {}
    }
    true
}
