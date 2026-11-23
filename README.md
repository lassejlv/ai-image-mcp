<img src="assets/logo.svg" width="96" alt="" />

# ai-image-mcp

An MCP server, written in Rust on the official [`rmcp`](https://github.com/modelcontextprotocol/rust-sdk) SDK, that lets any MCP client generate and edit images through the subscription or API key you already have:

| Provider type | Connect with | Default model |
| --- | --- | --- |
| `xai` | SuperGrok or X Premium+ login, or `XAI_API_KEY` | `grok-imagine-image-2.0` |
| `codex` | ChatGPT plan with Codex: own login or the Codex CLI's login | `gpt-image-2` |
| `openai` | `OPENAI_API_KEY`, or any OpenAI-compatible service via `base_url` | `gpt-image-2` |
| `gemini` | `GEMINI_API_KEY` | `gemini-nano-banana-2.1` |
| `fal` | `FAL_KEY`, with any model in fal's catalog | `fal-ai/flux/dev` |

Every request can pick the provider, the model, and the generation settings. Images are saved to disk and also returned inline to the client.

## Install

```sh
cargo install --git https://github.com/lassejlv/ai-image-mcp
```

Or run it with Docker (`linux/amd64` and `linux/arm64`, published on every release):

```sh
docker run -it --rm -v ai-image-mcp:/data ghcr.io/lassejlv/ai-image-mcp login xai
docker run -i --rm -v ai-image-mcp:/data -v ~/Pictures/ai-image-mcp:/home/nonroot/ai-image-mcp ghcr.io/lassejlv/ai-image-mcp serve
```

The `/data` volume keeps the config and saved logins. Generated images go to `/home/nonroot/ai-image-mcp` inside the container, so mount a host folder there that UID 65532 can write to. Pass API keys with `-e OPENAI_API_KEY`. Without arguments the container serves MCP over HTTP on port 8080 (`serve --http`, with `AI_IMAGE_MCP_BIND=0.0.0.0:8080`), which is what hosting platforms need; pass `serve` for stdio as above. HTTP on a public interface requires the sign-in from [Remote access](#remote-access-over-http-with-oauth), and `GET /health` answers `ok` for health checks.

## Connect a provider

Subscriptions use the device-code login, so it works over SSH and on headless machines too:

```sh
ai-image-mcp login xai      # SuperGrok / X Premium+
ai-image-mcp login codex    # ChatGPT plan with Codex
ai-image-mcp providers      # check what is connected
ai-image-mcp generate "a lighthouse at dawn, watercolor" --provider xai --aspect-ratio 16:9
```

If you already use the Codex CLI, `codex login` is enough: the `codex` provider falls back to `~/.codex/auth.json` (or `$CODEX_HOME`) and writes refreshed tokens back there so both tools stay signed in.

API-key providers read their key from the environment variable in the table above, or from `api_key_env` / `api_key` in the config file.

## Dashboard

```sh
ai-image-mcp dashboard            # http://127.0.0.1:7878
```

A small local web UI: connect xAI or ChatGPT with the same device login, see which providers are ready, generate with any provider, model, and setting (including reference images), and browse what has been saved. It listens on loopback only and refuses requests for any other host name.

## Add it to your MCP client

Claude Code:

```sh
claude mcp add ai-image -- ai-image-mcp
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.ai-image]
command = "ai-image-mcp"
```

Claude Desktop, Cursor, and other JSON-configured clients:

```json
{
  "mcpServers": {
    "ai-image": {
      "command": "ai-image-mcp",
      "env": { "OPENAI_API_KEY": "sk-..." }
    }
  }
}
```

## Remote access over HTTP with OAuth

`serve --http` serves MCP over streamable HTTP at `/mcp`, so clients on other machines (Claude, ChatGPT connectors, Cursor) can connect by URL. Set a username and password, and the server runs its own OAuth 2.1 authorization server, the flow MCP clients expect. Clients discover it, register themselves, and send you to a sign-in page:

```sh
export AI_IMAGE_MCP_AUTH_USERNAME=lasse
export AI_IMAGE_MCP_AUTH_PASSWORD='a long passphrase'
export AI_IMAGE_MCP_AUTH_SECRET="$(openssl rand -hex 32)"   # keeps sign-ins valid across restarts
ai-image-mcp serve --http --bind 0.0.0.0:8080 --public-url https://images.example.com
```

Then add `https://images.example.com/mcp` as a remote MCP server in your client.

The same server hosts the [dashboard](#dashboard) at `https://images.example.com/dashboard/`. It sits behind a sign-in with the same username and password, which sets an HttpOnly, SameSite=Strict session cookie that lasts 7 days.

| Variable / flag | Meaning |
| --- | --- |
| `AI_IMAGE_MCP_AUTH_USERNAME`, `AI_IMAGE_MCP_AUTH_PASSWORD` | The sign-in. Set both to require OAuth on `/mcp`. |
| `AI_IMAGE_MCP_AUTH_SECRET` | Key that signs client registrations and tokens. Without it a random key is used, and everyone signs in again after a restart. |
| `--bind` / `AI_IMAGE_MCP_BIND` | Listen address, default `127.0.0.1:8080`. |
| `--public-url` / `AI_IMAGE_MCP_PUBLIC_URL` | The URL clients use, when behind a proxy or TLS terminator. Defaults to `http://<bind>`. |

Access tokens last an hour, and refresh tokens last 30 days. Client ids, codes, and tokens are HMAC-signed, so nothing is stored server-side. PKCE (S256) is required, redirects must go to https, loopback, or an app's own URL scheme, and each failed sign-in is delayed by a second. Without credentials, the server refuses to listen on anything but loopback.

## Tools

**`generate_image`** creates or edits images. Only `prompt` is required.

| Argument | Meaning | Honoured by |
| --- | --- | --- |
| `prompt` | What to create, or the change to make | all |
| `provider`, `model` | Which configured provider and model to use | all |
| `input_images` | Paths, URLs, or data URIs to edit or use as references | all |
| `n` | Number of images (1-10) | all |
| `size` | `1024x1024`, `1536x1024`, `auto`, ... | openai, codex, fal |
| `aspect_ratio` | `1:1`, `16:9`, `9:16`, ... | xai, gemini, fal |
| `resolution` | `1k`, `2k`, `4k` | xai, gemini, fal |
| `quality` | `low`, `medium`, `high`, `auto` | xai (not `high`), openai, codex |
| `background` | `transparent`, `opaque`, `auto` | openai, codex |
| `output_format`, `output_compression` | `png` / `jpeg` / `webp`, 0-100 | openai (format also fal) |
| `seed`, `negative_prompt` | Diffusion controls | fal |
| `extra` | Any provider-specific field, merged into the request body | all |
| `output_dir`, `filename` | Where and under what name to save | all |
| `include_image_data` | Return image data inline (default from config) | all |

A setting the chosen provider does not support is reported back as not applied rather than silently dropped.

**`list_providers`** reports each provider's connection state, default and suggested models, supported settings, and how many input images it accepts.

## Configuration

Without a config file every built-in provider is offered and the first connected one is the default. To name providers, add OpenAI-compatible services, or set per-provider defaults, copy [`config.example.toml`](config.example.toml) to:

- macOS: `~/Library/Application Support/ai-image-mcp/config.toml`
- Linux: `~/.config/ai-image-mcp/config.toml`
- Windows: `%APPDATA%\ai-image-mcp\config.toml`

`AI_IMAGE_MCP_HOME` moves the whole directory (config and saved logins); `--config` or `AI_IMAGE_MCP_CONFIG` points at a specific file. Unknown keys are rejected, so typos fail loudly.

## Postgres storage

By default logins are saved under `<config dir>/ai-image-mcp/credentials/` and images in the output folder. Point the server at Postgres and both move into the database instead, so the server keeps no local state:

```sh
export AI_IMAGE_MCP_DATABASE_URL=postgres://user:password@localhost:5432/ai_image_mcp
ai-image-mcp login xai    # saved in the database
```

`database_url` in `config.toml` does the same; the environment variable wins. Tables are created and migrated on startup. Each image is stored with its prompt, provider, model, and settings. Tool results then name images by id (`image 42 in Postgres`) instead of by path, and include the image data inline as usual; the dashboard reads its gallery from the database. Token refreshes lock the login's row, so several server processes can share one database. The Codex CLI fallback still reads `~/.codex/auth.json`, since that file belongs to the Codex CLI.

## How the subscription logins work

- **xai** runs OAuth device-code login against `auth.x.ai` with xAI's public client for coding agents (the one Grok Build, Hermes, and OpenClaw use) and calls `api.x.ai/v1/images/*` with the resulting token. xAI has been seen to reject some SuperGrok tiers with HTTP 403; set `XAI_API_KEY` if that happens.
- **codex** runs the Codex CLI's device-code login against `auth.openai.com` and calls the same `chatgpt.com/backend-api/codex/images/*` endpoints the Codex CLI's image tool uses, so generations count against your ChatGPT plan instead of API credits.

Both are unofficial integrations of endpoints built for first-party tools and may change without notice. Saved logins live in `<config dir>/ai-image-mcp/credentials/` with `0600` permissions, and tokens are refreshed automatically (serialized, because refresh tokens rotate). `ai-image-mcp logout <xai|codex>` removes them.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The Postgres tests run when `AI_IMAGE_MCP_TEST_DATABASE_URL` points at a server whose user can create databases; each test gets a fresh one.

```
migrations/        SQL migrations, embedded and run on startup
src/
  main.rs          CLI: serve (stdio or --http), dashboard, login, logout, providers, generate
  remote.rs        MCP over streamable HTTP
  oauth/           built-in OAuth 2.1 server: metadata, registration, sign-in, tokens
  server.rs        MCP tools on rmcp
  app.rs           request pipeline shared by the server and the CLI
  config.rs        config.toml and on-disk locations
  image.rs         provider-neutral request, settings, and result types
  media.rs         loading input images, saving results
  http.rs          error and download helpers
  storage.rs       file or Postgres storage for images and logins
  auth/            API keys, OAuth grants, xAI and ChatGPT device logins
  providers/       one module per provider type, plus the registry
  dashboard/       local web dashboard: axum routes and static assets
tests/
  providers.rs     every provider against a mock HTTP server
  mcp.rs           an rmcp client driving the server end to end
  dashboard.rs     the dashboard's HTTP API
  postgres.rs      Postgres storage (needs AI_IMAGE_MCP_TEST_DATABASE_URL)
  oauth.rs         the HTTP transport's OAuth flow end to end
```
