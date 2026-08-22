# yap MVP

Status: first vertical slice implemented; hardening next

## Goal

Build a small native coding agent in Rust with a fullscreen Ratatui interface. Use a minimal, event-driven design while supporting the smallest useful coding workflow: inspect a workspace, propose edits, run commands with approval, and continue the model/tool loop.

## User flow

1. The user launches `yap` in a repository.
2. yap opens a transcript and prompt composer for the canonical current workspace.
3. The user submits a coding task.
4. Codex streams text and tool calls.
5. Read-only file tools run automatically inside the workspace.
6. Exact-match file changes are applied automatically inside the workspace.
7. Shell commands show the exact command, working directory, timeout, and risk notice, then wait for explicit approval.
8. Tool results return to Codex until it answers, is cancelled, or reaches a limit.

## Provider

- OpenAI Responses API
- Model configurable; initial default: `gpt-5.3-codex`
- Authentication: `OPENAI_API_KEY` environment variable
- Optional endpoint override: `OPENAI_BASE_URL`
- ChatGPT/Codex OAuth is out of scope for the MVP
- Transport frames are normalized into provider-independent model events

## MVP capabilities

### Interface

- Fullscreen Ratatui transcript
- Single prompt composer
- Streaming assistant text and tool status
- Scrollback
- Approval modal
- Cancellation
- Visible errors and step usage

### Tools

- `list_files`: bounded workspace listing
- `read_file`: bounded workspace-relative reads
- `apply_patch`: automatic exact-match replacement inside the workspace
- `run_command`: approval before execution, with timeout and output limits

### Safety

- One canonical workspace root
- Relative file paths only; reject traversal and out-of-root access
- Read-only tools may run automatically
- Shell commands require a typed UI decision tied to their tool-call ID
- Model text and replayed session data never grant authority
- Bound model steps, tool calls, file reads, command output, and retained transcript
- Commands run with closed stdin and are killed on timeout or cancellation
- Do not claim OS-level sandboxing

## Modules and seams

```text
CLI/TUI -> Agent -> Model
              |-> ToolRegistry -> Workspace / CommandRunner
              |-> ApprovalBroker -> TUI
```

- `Agent::run_turn`: public behavior seam for the sequential model/tool loop
- `Model::stream`: provider seam; implemented by OpenAI Responses and a scripted fake
- `Tool::execute`: typed tool seam behind decode, validation, policy, and execution
- `ApprovalBroker::decide`: human-authority seam; implemented by Ratatui and a scripted fake
- Ratatui consumes `AgentEvent`; the agent never renders

Persistence is deferred, so no `SessionStore` seam exists in the MVP.

## Acceptance criteria

### Protocol spike

- A CLI call streams one Codex response through the OpenAI Responses API.
- Fragmented SSE events are parsed correctly.
- Provider errors, malformed events, cancellation, and incomplete streams produce typed failures.
- Protocol behavior is testable with recorded fixtures and no live API call.

### Vertical slice

- A scripted fake model can drive: user prompt -> file read -> automatic patch -> command approval -> final response.
- Denied tools return a denial result to the model without performing the effect.
- Unknown tools and invalid arguments never reach approval or execution.
- Workspace traversal is rejected.
- Limits and cancellation stop work predictably.
- Terminal state is restored after normal exit, error, cancellation, or panic.

## Non-goals

- Multiple providers
- ChatGPT OAuth
- Sessions/resume
- Parallel or background tools
- MCP, skills, subagents, browser, vision, or web search
- Additional workspaces
- Automatic permission review or persistent approval rules
- OS sandboxing
- Full Markdown rendering or syntax highlighting

## Current state

Completed:

- OpenAI Responses streaming and normalized model events
- Sequential model/tool loop with bounded steps
- Fullscreen Ratatui transcript, composer, approval flow, and active-turn cancellation
- Workspace-scoped `list_files` and `read_file`
- Automatic workspace-scoped `apply_patch` with exact-match validation
- Approval-gated `run_command` with timeout and bounded output
- Integration tests for provider streaming, agent behavior, tools, and workspace escapes
- End-to-end fake-provider flow covering read -> automatic edit -> approved command -> final response

Known gaps:

- No CI pipeline
- No session persistence or resume
- No multiline composer or rich Markdown/diff rendering
- Cancellation and descendant-process cleanup need more adversarial testing
- Filesystem checks are canonical-path based, not capability based
- No OS sandbox

## Roadmap

### 1. Harden the vertical slice

- [x] Add an end-to-end test using a fake provider
- [x] Cancel active model streams, pending approvals, and running commands without exiting
- [ ] Expand malformed stream, terminal restoration, and descendant-process cleanup tests
- [ ] Move file access to capability-rooted operations and test filesystem races
- [ ] Add redaction and explicit bounds for every retained buffer

CI automation is deferred for now; formatting, tests, and Clippy remain local release checks.

### 2. Improve daily usability

- Add multiline editing and better transcript navigation
- Improve Markdown, diff, and tool-output rendering
- Add Git status and diff context
- Surface model, step, token, and error details clearly

### 3. Add persistence and configuration

- Store semantic session events as private JSONL
- Resume sessions by validated ID and workspace
- Add validated global and per-project configuration
- Keep credentials environment-only

### 4. Add extensibility

- Add a second provider to validate the model seam
- Support custom tools and configurable prompts
- Evaluate skills and MCP after the core remains reliable

### 5. Prepare releases

- Publish versioned, prebuilt binaries and an installer
- Add a changelog, security policy, and contribution guide
- Document platform support and sandbox guarantees precisely

The immediate priority is Stage 1. New feature breadth waits until the current vertical slice is reliable.
