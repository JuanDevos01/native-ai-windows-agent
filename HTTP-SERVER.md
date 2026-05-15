# Local HTTP agent API (`metis serve`)

Metis can expose the same **agent loop** used by the CLI over a small **Axum** HTTP API. This is useful for Windows (or any OS) when another process, script, or UI needs to send prompts without spawning `Metis agent` each time.

## Quick start

```bash
cargo build --release -p metis-cli
target/release/metis.exe serve
```

Defaults:

- **Bind:** `127.0.0.1:18791` (loopback; does not use the same port as `gateway.port` / Docker `18790` by default)
- **Auth:** none until you set a token (see below)

## Endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/health` | No | Liveness JSON `{"status":"ok"}` |
| `GET` | `/v1/status` | If configured | Model id and channel hint |
| `POST` | `/v1/chat` | If configured | Run one agent turn |

### `POST /v1/chat`

Request body (JSON):

```json
{
  "message": "What is 2+2?",
  "session": "optional-session-id"
}
```

- `session` is optional. When omitted, the session id `default` is used (isolated history under `http:<session>`).

Response (JSON):

```json
{ "response": "..." }
```

Errors return JSON `{ "error": "..." }` with `4xx` / `5xx` as appropriate.

## Configuration (`~/.metis/config.json`)

```json
{
  "httpServer": {
    "host": "127.0.0.1",
    "port": 18791,
    "apiKey": ""
  }
}
```

- **`apiKey`:** when non-empty, every `/v1/*` request must send `Authorization: Bearer <apiKey>`. `/health` stays unauthenticated.

## CLI overrides

```bash
metis serve --host 127.0.0.1 --port 18791 --api-key YOUR_SECRET --logs
```

## Environment overrides

Same pattern as other Metis settings:

- `METIS_HTTP_SERVER__HOST`
- `METIS_HTTP_SERVER__PORT`
- `METIS_HTTP_SERVER__API_KEY`

## Security notes

- Prefer **`127.0.0.1`** unless you intentionally expose the API on the LAN.
- If you bind to **`0.0.0.0`**, set **`httpServer.apiKey`** (or `--api-key`). Metis logs a warning if you expose all interfaces without a token.
- This server does **not** implement TLS; put a reverse proxy in front if you need HTTPS.

## Tauri (optional later)

A **Tauri** desktop shell can call this same API at `http://127.0.0.1:<port>` or embed a webview pointed at it. You do not need Tauri to use `metis serve`.
