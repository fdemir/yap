# Configuration and provider system

Status: implemented

## Goal

Provide a small OpenCode-like configuration surface for selecting `provider/model`, defining provider connections, and setting per-model options without coupling the agent loop to a provider.

## Terms

- **Provider profile**: a user-named connection such as `openai`, `openrouter`, or `ollama`.
- **Protocol adapter**: compiled wire support behind `Model`; currently `openai-responses` and `openai-chat`.
- **Model reference**: `provider-profile/model-id`, split only on the first slash.
- **Selected model**: the reference and settings fixed for one turn.

## Format

Yap uses strict JSONC with comments, trailing commas, unknown-field rejection, and a published schema.

```jsonc
{
  "$schema": "https://raw.githubusercontent.com/fdemir/yap/main/schema/config.schema.json",
  "version": 1,
  "model": "openrouter/anthropic/claude-sonnet-4.5",
  "provider": {
    "openrouter": {
      "name": "OpenRouter",
      "protocol": "openai-chat",
      "options": {
        "baseURL": "https://openrouter.ai/api/v1",
        "apiKeyEnv": "OPENROUTER_API_KEY",
        "timeoutMs": 300000,
        "streamIdleTimeoutMs": 30000,
        "headers": {
          "HTTP-Referer": "https://example.com",
          "X-Title": "yap"
        }
      },
      "models": {
        "anthropic/claude-sonnet-4.5": {
          "name": "Claude Sonnet 4.5",
          "options": {
            "temperature": 0.2,
            "maxOutputTokens": 32000
          }
        }
      }
    }
  }
}
```

OpenAI Responses models additionally support `reasoningEffort` and `textVerbosity`.

## Sources and precedence

Later sources override earlier sources:

1. built-in OpenAI defaults;
2. global config;
3. project config;
4. explicit `YAP_CONFIG` file;
5. `OPENAI_BASE_URL`, `YAP_MODEL`, and legacy `OPENAI_MODEL` environment overrides.

Locations:

- Unix/macOS global: `$XDG_CONFIG_HOME/yap/config.jsonc`, otherwise `~/.config/yap/config.jsonc`;
- Windows global: `%APPDATA%\yap\config.jsonc`;
- project: `<workspace>/yap.jsonc`;
- explicit: `YAP_CONFIG=/path/to/config.jsonc`.

`.json` is accepted when the corresponding `.jsonc` file does not exist.

No config preserves the current defaults:

- `openai/gpt-5.3-codex`;
- `openai-responses`;
- `https://api.openai.com/v1`;
- `OPENAI_API_KEY`.

## Merge and selection

- provider and model maps merge by key;
- nested objects merge; later scalar settings replace earlier values;
- arrays are not part of version 1;
- model IDs may contain additional slashes;
- undeclared models may be selected; a model entry only supplies a display name and options;
- missing provider profiles or credentials fail before terminal startup;
- there is no provider fallback or automatic routing.

## Trust and secrets

Credentials are never values in config. `apiKeyEnv` is a credential reference resolved late into `SecretString` only for the selected profile.

Project config may select a model and add/override model metadata/options. It may not set provider protocol, base URL, credential environment, headers, or network timeout fields. This prevents a cloned repository from redirecting a globally available credential and workspace context.

Additional checks:

- remote endpoints require HTTPS;
- loopback endpoints may use HTTP for local models;
- URLs may not contain credentials, query strings, or fragments;
- custom headers cannot replace authorization, host, content length/type, or provider auth headers;
- config size, provider/model counts, identifiers, headers, timeouts, and model values are bounded;
- errors do not include credential values.

## Modules

### `Configuration`

```rust
Configuration::load(workspace: &Path) -> Result<ResolvedConfig, ConfigError>
```

This deep module owns bounded reads, JSONC parsing, source trust, merge precedence, compatibility environment variables, validation, and diagnostics.

### `ProviderSystem`

```rust
ProviderSystem::new(config: ResolvedConfig)
ProviderSystem::select(model: &ModelRef) -> Result<SelectedModel, ProviderError>
```

This deep module owns profile lookup, late credential resolution, header/client construction, endpoint selection, option validation, and protocol-adapter construction.

The existing provider-independent `Model` interface remains the seam consumed by `Agent`. `Agent` can receive `Box<dyn Model>`, so scripted fakes and production adapters use the same interface.

## Supported protocols

### `openai-responses`

Uses the existing bounded Responses SSE normalizer and supports tool calls, cancellation, custom headers, optional authentication, request/idle timeouts, reasoning effort, text verbosity, temperature, and maximum output tokens.

### `openai-chat`

Uses Chat Completions-compatible messages and SSE. It supports text streaming, fragmented tool-call assembly, tool outputs, cancellation, custom headers, optional authentication, request/idle timeouts, temperature, and maximum completion tokens.

This covers OpenRouter-style, Ollama-style, LM Studio-style, and similar compatible profiles without dynamic provider packages.

## Deferred

- native Anthropic Messages and Google adapters;
- OAuth and stored credentials;
- remote model catalogs or model discovery;
- TUI model picker and between-turn switching;
- last-used model persistence;
- remote/managed organization configuration;
- automatic fallback or routing.

## Acceptance criteria

- No config behaves exactly like the previous OpenAI setup.
- Global JSONC can define and select a Responses or Chat-compatible provider.
- Project config cannot redirect provider connections or credentials.
- `provider/model` preserves slashes inside the model ID.
- Provider credentials are resolved only for the selected profile.
- Invalid config, missing credentials, and unsupported options fail before the first request.
- Responses and Chat adapters normalize text/tool streaming into the same `ModelEvent` types.
- The footer displays the complete selected model reference.
