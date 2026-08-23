# yap

An experimental native coding agent built with Rust and Ratatui.

## Run

```bash
export OPENAI_API_KEY="..."
cargo run
```

Optional: `OPENAI_MODEL` and `OPENAI_BASE_URL`.

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
