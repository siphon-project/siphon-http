# WhatsApp Cloud API

The [WhatsApp Cloud API](https://developers.facebook.com/docs/whatsapp/cloud-api)
is plain HTTP: you `POST` messages to Meta's Graph API, and Meta `POST`s inbound
messages and delivery statuses back to a webhook you host. Those are exactly the
two halves of the http addon — the pooled `http.Client` for the outbound Graph
calls, and `@http.route` for the inbound webhook — so a full bridge is a script,
with no protocol code.

This mirrors
[`examples/whatsapp_cloud_api.py`](https://github.com/siphon-project/siphon-http/blob/main/examples/whatsapp_cloud_api.py).

## Inbound webhook

Meta first does a **GET verify handshake**, then **POST**s events signed with an
HMAC over the raw body. Both live on the same path:

```python
import hashlib, hmac, json, os
from siphon import http, cache, log

APP_SECRET = os.environ.get("WHATSAPP_APP_SECRET", "")
VERIFY_TOKEN = os.environ.get("WHATSAPP_VERIFY_TOKEN", "")


def verify_signature(app_secret, raw_body, signature_header):
    # header is "sha256=<hex>", hex = HMAC-SHA256(app_secret, RAW body)
    if not signature_header or not signature_header.startswith("sha256="):
        return False
    expected = hmac.new(app_secret.encode(), raw_body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature_header[len("sha256="):])


@http.route("/whatsapp/webhook", methods=["GET"])
async def verify_webhook(req):
    p = req.query_params
    if p.get("hub.mode") == "subscribe" and p.get("hub.verify_token") == VERIFY_TOKEN:
        return http.Response(status=200, body=p.get("hub.challenge", "").encode())
    return http.Response(status=403, body=b"forbidden\n")


@http.route("/whatsapp/webhook", methods=["POST"])
async def receive_webhook(req):
    raw = req.body() or b""
    if not verify_signature(APP_SECRET, raw, req.header("x-hub-signature-256")):
        return http.Response(status=403, body=b"bad signature\n")
    payload = json.loads(raw or b"{}")
    # ... iterate payload["entry"][*]["changes"][*]["value"]["messages"/"statuses"]
    return http.Response(status=200)     # 2xx fast so Meta does not re-deliver
```

Two things are load-bearing:

- **Verify the signature over the RAW bytes.** `req.body()` returns the exact
  buffered bytes Meta signed; re-serialising the parsed JSON would change the
  bytes and break the HMAC. Compare in constant time (`hmac.compare_digest`).
- **Answer 2xx quickly.** Meta re-delivers a webhook until it gets a `200`, so do
  slow work after responding (or off a background task), and **dedup on the
  message id** — Meta may deliver the same message more than once:

```python
async def already_seen(message_id):
    if await cache.exists("wa_dedup", message_id):
        return True
    await cache.store("wa_dedup", message_id, "1", ttl=3600)
    return False
```

Back the `wa_dedup` cache with Redis to dedup across replicas; a local LRU dedups
within one process.

## Outbound (Graph API)

A named, pooled client sends the Graph POST. The access token is a per-request
`Authorization: Bearer` header:

```python
TOKEN = os.environ.get("WHATSAPP_TOKEN", "")
PHONE_NUMBER_ID = os.environ.get("WHATSAPP_PHONE_NUMBER_ID", "")


async def send_text(to, text):
    body = json.dumps({
        "messaging_product": "whatsapp",
        "to": to,
        "type": "text",
        "text": {"body": text},
    }).encode()
    headers = {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"}
    async with http.Client("graph") as client:
        response = await client.post(f"/{PHONE_NUMBER_ID}/messages", body=body, headers=headers)
    return response.status
```

Messages outside the 24-hour customer-service window must be **templates** —
`build_template_message` in the example builds those.

## Config

```yaml
# http.yaml (referenced from siphon.yaml `extensions.http`)
clients:
  graph:
    base_url: "https://graph.facebook.com/v20.0"   # version lives in the base URL
    timeout_ms: 5000
servers:
  - listen: "0.0.0.0:8443"
    max_body_bytes: 262144
    tls:                          # or terminate TLS at a fronting LB (Meta needs HTTPS)
      cert_path: "/etc/siphon/tls/webhook.crt"
      key_path: "/etc/siphon/tls/webhook.key"
```

```yaml
# siphon.yaml
script:
  path: "examples/whatsapp_cloud_api.py"
cache:
  - name: "wa_dedup"
    url: "redis://localhost:6379"    # omit `url` for a process-local LRU
    local_ttl_secs: 3600
extensions:
  http: http.yaml
```

Keep secrets out of the files — the example reads `WHATSAPP_TOKEN`,
`WHATSAPP_APP_SECRET`, `WHATSAPP_VERIFY_TOKEN`, and `WHATSAPP_PHONE_NUMBER_ID`
from the environment.

## Try it

```bash
# Meta's verify handshake (should echo the challenge value):
curl -sS "localhost:8443/whatsapp/webhook?hub.mode=subscribe&hub.verify_token=$WHATSAPP_VERIFY_TOKEN&hub.challenge=1234"

# A signed event POST:
BODY='{"entry":[{"changes":[{"value":{"messages":[{"id":"wamid.1","from":"15551234567","type":"text","text":{"body":"hi"}}]}}]}]}'
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WHATSAPP_APP_SECRET" -r | cut -d' ' -f1)
curl -sS -XPOST localhost:8443/whatsapp/webhook \
     -H "x-hub-signature-256: sha256=$SIG" -H 'content-type: application/json' -d "$BODY"
```

## See also

- **WhatsApp voice.** WhatsApp also does voice, over SIP (the Business Calling
  API). That is a separate, SIP-side integration handled by siphon itself — see
  the [WhatsApp calling](https://siphon-sip.org/cookbook/whatsapp-calling/)
  cookbook.
- **SMS.** siphon also ships an `smpp` addon. The same compose-a-binary +
  script pattern lets you gateway messages between WhatsApp and SMS — but confirm
  Meta's WhatsApp Business terms permit routing WhatsApp content to SMS/SMPP
  before you build that; it is commonly restricted.
