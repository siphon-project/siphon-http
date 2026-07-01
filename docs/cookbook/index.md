# Cookbook

Complete, runnable starting points for common HTTP roles. Each recipe is a
single routing script plus the `http.yaml` listener it needs — point your siphon
build's `script.path` at the file, reference the `http.yaml` under
`extensions.http`, and it runs.

- **[Webhook receiver](webhook.md)** — a token-guarded `POST` endpoint that
  validates a bearer token in middleware and echoes a JSON acknowledgement.
- **[REST API](rest-api.md)** — path params, multiple methods on one route, query
  params, and JSON bodies over a tiny in-memory resource.
- **[Reverse proxy](proxy.md)** — a catch-all route that forwards to an upstream
  API over a named, pooled `http.Client`.

Each of these has a matching file under
[`examples/`](https://github.com/siphon-project/siphon-http/tree/main/examples)
in the repo.

For the objects and decorators the recipes use, see the
[Script API](../script-api.md); for the listener and client config, see
[Configuration](../configuration.md).
