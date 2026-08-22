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
- `Esc`: cancel the active turn
- `d`: deny a pending command
- `↑` / `↓`: scroll
- `Ctrl+C`: exit

> Approved shell commands run with your user permissions. There is no OS sandbox.

## Development

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## License

MIT
