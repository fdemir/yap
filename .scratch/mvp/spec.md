# yap MVP

Status: Stage 1 hardening complete; daily usability in progress

## Goal

Build a small native coding agent in Rust with a fullscreen Ratatui interface. Use a minimal, event-driven design while supporting the smallest useful coding workflow: inspect a workspace, apply edits, run commands with contextual approval, and continue the model/tool loop.

## User flow

1. The user launches `yap` in a repository.
2. yap opens a transcript and prompt composer for the canonical current workspace.
3. The user submits a coding task.
4. Codex streams text and tool calls.
5. Read-only file tools run automatically inside the workspace, except sensitive `.env` reads.
6. Exact-match file changes are applied automatically inside the workspace.
7. Shell commands run automatically when their detected paths remain inside the workspace; external access waits for explicit approval.
8. A third consecutive tool call with the same arguments waits for explicit approval.
9. Tool results return to Codex until it answers, is cancelled, or reaches a limit.

## Provider

- OpenAI Responses and OpenAI Chat-compatible protocols
- Models selected as `provider/model`; default: `openai/gpt-5.3-codex`
- Global and project JSONC configuration with environment-only credentials
- Custom profiles for compatible endpoints such as OpenRouter, Ollama, and LM Studio
- ChatGPT/Codex OAuth is out of scope for the MVP
- Transport frames are normalized into provider-independent model events

## MVP capabilities

### Interface

- Fullscreen Ratatui transcript
- Multiline prompt composer with cursor movement and in-memory history
- Streaming assistant text and tool status
- Wrap-aware scrollback with automatic tail following and manual navigation
- Approval modal
- Cancellation
- Visible errors and step usage

### Tools

- `list_files`: bounded workspace listing
- `read_file`: bounded workspace-relative reads
- `apply_patch`: automatic exact-match replacement inside the workspace
- `run_command`: automatic workspace execution with external-path approval, timeout, and output limits

### Safety

- One canonical workspace root
- Relative file paths only; reject traversal and out-of-root access
- Read-only tools run automatically except sensitive `.env` reads
- Workspace edits and ordinary workspace commands run automatically
- Sensitive reads, detected external command paths, and repeated tool calls require a typed UI decision tied to their tool-call ID
- Direct file-tool traversal and out-of-root access remain blocked
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
- `Model::stream`: provider seam; implemented by OpenAI Responses, OpenAI Chat, and a scripted fake
- `Tool::execute`: typed tool seam behind decode, validation, policy, and execution
- `ApprovalBroker::decide`: human-authority seam; implemented by Ratatui and a scripted fake
- Ratatui consumes `AgentEvent`; the agent never renders

Persistence is deferred, so no `SessionStore` seam exists in the MVP.

## Acceptance criteria

### Protocol spike

- A CLI call streams one response through the selected configured provider.
- Fragmented Responses and Chat SSE events are parsed correctly.
- Provider errors, malformed events, cancellation, and incomplete streams produce typed failures.
- Protocol behavior is testable with recorded fixtures and no live API call.

### Vertical slice

- A scripted fake model can drive: user prompt -> file read -> automatic patch -> automatic workspace command -> final response.
- Denied tools return a denial result to the model without performing the effect.
- Unknown tools and invalid arguments never reach approval or execution.
- Workspace traversal is rejected.
- Limits and cancellation stop work predictably.
- Terminal state is restored after normal exit, error, cancellation, or panic.

## Non-goals

- Native non-OpenAI-compatible providers
- ChatGPT OAuth
- Sessions/resume
- Parallel or background tools
- MCP, skills, subagents, browser, vision, or web search
- Additional workspaces
- Configurable or persistent approval rules
- OS sandboxing
- Full Markdown rendering or syntax highlighting

## Current state

Completed:

- OpenAI Responses and OpenAI Chat-compatible streaming with normalized model events
- Layered JSONC configuration, provider profiles, per-model options, and environment-only credentials
- Sequential model/tool loop with bounded steps
- Fullscreen Ratatui transcript, multiline composer, approval flow, and active-turn cancellation
- Workspace-scoped `list_files` and `read_file`
- Automatic workspace-scoped `apply_patch` with exact-match validation
- Automatic workspace `run_command` with external-path approval, timeout, and bounded output
- Approval for sensitive `.env` reads and repeated identical tool calls
- Strict bounded SSE framing with sequence and tool-lifecycle validation
- Identity-checked process-tree cleanup across escaped sessions/process groups on Linux and macOS
- TERM grace followed by forced cleanup on cancellation, timeout, and output floods
- Early RAII terminal restoration with panic and TERM/HUP/Ctrl+C shutdown coverage
- Capability-rooted, final-component-no-follow file access and atomic, stale-content-checked patch replacement
- Explicit prompt, stream, argument, output, patch, listing, transcript, and error limits
- Central secret redaction for tool output, approvals, structured arguments, and displayed errors
- Terminal-safe escaping for control, invisible, and bidirectional-override characters
- Cursor-aware multiline editing, bounded paste, and bounded in-memory prompt history
- Markdown transcript rendering with styled code blocks, diffs, lists, tables, and bounded tool-output previews
- Wrap-aware transcript navigation with stable manual position and automatic tail following
- Integration tests for provider streaming, agent behavior, tools, and workspace escapes
- End-to-end fake-provider flow covering read -> automatic edit -> automatic command -> final response

Known gaps:

- No CI pipeline
- No session persistence or resume
- No syntax-highlighted code rendering or expandable full tool-output view
- No OS sandbox; shell external-path detection is best effort
- Process identity tracking is Linux/macOS-specific; Windows uses `taskkill /T` and other Unix targets retain process-group cleanup
- Secret detection is heuristic and cannot recognize every credential shape
- Atomic replacement still has an unavoidable final namespace race

## Roadmap

### 1. Harden the vertical slice

- [x] Add an end-to-end test using a fake provider
- [x] Cancel active model streams, pending approvals, and running commands without exiting
- [x] Expand malformed stream, terminal restoration, and descendant-process cleanup tests
- [x] Move file access to capability-rooted operations and test filesystem races
- [x] Add redaction and explicit bounds for every retained buffer

CI automation is deferred for now; formatting, tests, and Clippy remain local release checks.

### 2. Improve daily usability

- [x] Add multiline editing, bounded paste, and prompt history navigation
- [x] Improve transcript navigation
- [x] Add global/project configuration and selectable provider profiles
- [x] Improve Markdown, diff, and tool-output rendering
- [ ] Add Git status and diff context
- [ ] Surface model, step, token, and error details clearly

### 3. Add persistence

- Store semantic session events as private JSONL
- Resume sessions by validated ID and workspace
- Keep credentials environment-only

### 4. Add extensibility

- Add a native non-OpenAI-compatible provider to further validate the model seam
- Support custom tools and configurable prompts
- Evaluate skills and MCP after the core remains reliable

### 5. Prepare releases

- Publish versioned, prebuilt binaries and an installer
- Add a changelog, security policy, and contribution guide
- Document platform support and sandbox guarantees precisely

Stage 1 is complete. The immediate priority is Stage 2 daily usability.
