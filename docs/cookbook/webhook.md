# Webhook receiver

A token-guarded webhook endpoint. It accepts `POST`s to `/webhook`, rejects any
request without the right bearer token via a middleware guard, parses the JSON
body, and echoes a small acknowledgement.

This mirrors
[`examples/webhook.py`](https://github.com/siphon-project/siphon-http/blob/main/examples/webhook.py).

## Script

```python
import json

from siphon import http, log

TOKEN = "Bearer s3cr3t"


@http.middleware
async def require_token(req):
    # Runs before every route. Return a Response to short-circuit; None to
    # continue. A real deployment reads the token from config / a secret store.
    if req.header("authorization") != TOKEN:
        return http.Response(status=401, body=b"unauthorized\n")
    return None


@http.route("/webhook", methods=["POST"])
async def receive(req):
    raw = req.body()
    try:
        event = json.loads(raw or b"{}")
    except ValueError:
        return http.Response(status=400, body=b"invalid json\n")

    log.info(f"webhook from {req.client}: {event}")
    ack = json.dumps({"ok": True, "received": event.get("event")}).encode()
    return http.Response(status=200, headers={"Content-Type": "application/json"}, body=ack)
```

## Listener

A single plain-HTTP listener is enough for a local run:

```yaml
# http.yaml
servers:
  - listen: "127.0.0.1:8080"
    max_body_bytes: 65536
```

For anything internet-facing, terminate TLS on the listener — see
[Configuration → TLS termination](../configuration.md#tls-termination).

## Try it

```bash
curl -sS -XPOST 127.0.0.1:8080/webhook \
     -H 'authorization: Bearer s3cr3t' \
     -d '{"event": "ping"}'
```

A request without the header — or with the wrong token — gets a `401` from the
middleware and never reaches `receive`.

## How it works

- The **`@http.middleware`** guard runs before every route. Returning a
  `Response` short-circuits; returning `None` lets the request continue. See
  [Script API → `@http.middleware`](../script-api.md#httpmiddleware).
- **`req.body()`** returns the buffered request body as bytes; the body is capped
  by `max_body_bytes` (over-cap requests get a `413` before the handler runs).
- **`req.client`** is the remote `ip:port`, handy for logging.

!!! tip
    Keep the token out of the script in production — read it from an environment
    variable via [config expansion](../configuration.md#environment-expansion) or
    a secret store, not a literal.
