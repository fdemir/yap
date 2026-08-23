# yap

An experimental native coding agent built with Rust and Ratatui.

## Run

```bash
export OPENAI_API_KEY="..."
cargo run
```

Optional: `YAP_MODEL`, `YAP_CONFIG`, `OPENAI_MODEL`, and `OPENAI_BASE_URL`.

## Configuration

Yap merges `~/.config/yap/config.jsonc` and `<workspace>/yap.jsonc`. Models use `provider/model` IDs. Provider connections belong in the global config; credentials remain in environment variables.

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/fdemir/yap/main/schema/config.schema.json",
  "model": "openrouter/anthropic/claude-sonnet-4.5",
  "provider": {
    "openrouter": {
      "protocol": "openai-chat",
      "options": {
        "baseURL": "https://openrouter.ai/api/v1",
        "apiKeyEnv": "OPENROUTER_API_KEY"
      }
    }
  }
}
```

Supported protocols: `openai-responses` and `openai-chat`.

## Controls

- `Enter`: submit or approve
- `Alt+Enter` (`Shift+Enter` where supported): newline
- `←` / `↑` / `↓` / `→`, `Home` / `End`: edit
- `Ctrl+P` / `Ctrl+N`: prompt history
- `PageUp` / `PageDown`: scroll transcript
- `Ctrl+Home` / `Ctrl+End`: transcript top / live tail
- `Esc`: cancel the active turn
- `d`: deny a pending approval
- `Ctrl+C`: exit

> File tools are workspace-confined. Shell commands run with your user permissions, and most workspace commands do not require approval. There is no OS sandbox.

## Development

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## License

MIT
