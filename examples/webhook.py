"""
webhook.py — a token-guarded webhook receiver.

Point your siphon build's script path at this file and configure an `http`
listener (see harness/http.yaml). It accepts POSTs to /webhook, rejects any
request without the right bearer token via a middleware guard, and echoes a
small JSON acknowledgement.

    curl -sS -XPOST localhost:8080/webhook \
         -H 'authorization: Bearer s3cr3t' \
         -d '{"event": "ping"}'
"""

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
