"""
whatsapp_cloud_api.py — a WhatsApp Cloud API messaging bridge on siphon-http.

The WhatsApp Cloud API is plain HTTP: you POST messages to Meta's Graph API and
Meta POSTs inbound messages + delivery statuses back to a webhook you host. That
maps directly onto the two halves of the http addon — the pooled `http.Client`
for the outbound Graph calls, and `@http.route` for the inbound webhook — so a
full bridge is a script, with no protocol code in Rust.

    outbound:  http.Client("graph").post("/{phone_number_id}/messages", …)
    inbound:   GET  /whatsapp/webhook   (Meta's verify handshake)
               POST /whatsapp/webhook   (messages + statuses, HMAC-signed)

Environment:
    WHATSAPP_TOKEN            Graph API access token (Bearer)
    WHATSAPP_APP_SECRET       App secret — verifies the X-Hub-Signature-256 HMAC
    WHATSAPP_VERIFY_TOKEN     Shared token you set when subscribing the webhook
    WHATSAPP_PHONE_NUMBER_ID  The sending phone number's Graph node id

Config (http.yaml, referenced from siphon.yaml `extensions.http`):
    clients:
      graph:
        base_url: "https://graph.facebook.com/v20.0"
    servers:
      - listen: "0.0.0.0:8443"      # terminate TLS here or at a fronting LB
        tls: { cert_path: …, key_path: … }

WhatsApp also does *voice* over SIP (the Business Calling API). That is a
separate, SIP-side integration — see the "WhatsApp calling" cookbook in the
siphon docs (https://siphon-sip.org/cookbook/whatsapp-calling/).

    curl -sS "localhost:8443/whatsapp/webhook?hub.mode=subscribe&hub.verify_token=$WHATSAPP_VERIFY_TOKEN&hub.challenge=1234"
"""
import hashlib
import hmac
import json
import os

from siphon import http, cache, log

TOKEN = os.environ.get("WHATSAPP_TOKEN", "")
APP_SECRET = os.environ.get("WHATSAPP_APP_SECRET", "")
VERIFY_TOKEN = os.environ.get("WHATSAPP_VERIFY_TOKEN", "")
PHONE_NUMBER_ID = os.environ.get("WHATSAPP_PHONE_NUMBER_ID", "")

# Inbound message-id dedup (Meta re-delivers a webhook until it gets a 2xx). A
# named cache from siphon.yaml `cache:` — back it with Redis to dedup across
# replicas; a local LRU dedups within one process.
DEDUP_CACHE = "wa_dedup"
DEDUP_TTL_SECS = 3600


# --------------------------------------------------------------------------
# Pure helpers — no I/O, unit-testable without a running server.
# --------------------------------------------------------------------------
def verify_signature(app_secret: str, raw_body: bytes, signature_header: str | None) -> bool:
    """Verify Meta's X-Hub-Signature-256 over the RAW request body.

    The header is ``sha256=<hex>`` where <hex> is HMAC-SHA256(app_secret, body).
    Compared in constant time. Returns False on a missing/malformed header.
    """
    if not signature_header or not signature_header.startswith("sha256="):
        return False
    provided = signature_header[len("sha256="):]
    expected = hmac.new(app_secret.encode(), raw_body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, provided)


def build_text_message(to: str, text: str, preview_url: bool = False) -> dict:
    """Build a Cloud API text-message body."""
    return {
        "messaging_product": "whatsapp",
        "recipient_type": "individual",
        "to": to,
        "type": "text",
        "text": {"preview_url": preview_url, "body": text},
    }


def build_template_message(to: str, template: str, lang: str = "en_US",
                           components: list | None = None) -> dict:
    """Build a Cloud API template-message body (for messages outside the 24h window)."""
    message = {
        "messaging_product": "whatsapp",
        "to": to,
        "type": "template",
        "template": {"name": template, "language": {"code": lang}},
    }
    if components:
        message["template"]["components"] = components
    return message


def iter_inbound_messages(payload: dict):
    """Yield each inbound message dict from a webhook payload (skips status-only)."""
    for entry in payload.get("entry", []):
        for change in entry.get("changes", []):
            for message in change.get("value", {}).get("messages", []):
                yield message


def iter_status_updates(payload: dict):
    """Yield each delivery-status dict (sent/delivered/read/failed) from a payload."""
    for entry in payload.get("entry", []):
        for change in entry.get("changes", []):
            for status in change.get("value", {}).get("statuses", []):
                yield status


# --------------------------------------------------------------------------
# Inbound webhook (Meta -> us).
# --------------------------------------------------------------------------
@http.route("/whatsapp/webhook", methods=["GET"])
async def verify_webhook(req):
    """Meta's subscription verify handshake: echo hub.challenge if the token matches."""
    params = req.query_params
    if params.get("hub.mode") == "subscribe" and params.get("hub.verify_token") == VERIFY_TOKEN:
        return http.Response(status=200, body=params.get("hub.challenge", "").encode())
    return http.Response(status=403, body=b"forbidden\n")


@http.route("/whatsapp/webhook", methods=["POST"])
async def receive_webhook(req):
    raw = req.body() or b""
    if not verify_signature(APP_SECRET, raw, req.header("x-hub-signature-256")):
        log.warn("whatsapp webhook: bad or missing X-Hub-Signature-256")
        return http.Response(status=403, body=b"bad signature\n")
    try:
        payload = json.loads(raw or b"{}")
    except ValueError:
        return http.Response(status=400, body=b"invalid json\n")

    for message in iter_inbound_messages(payload):
        message_id = message.get("id")
        if message_id and await _already_seen(message_id):
            continue                                    # Meta retries — skip dupes
        await _handle_message(message)

    for status in iter_status_updates(payload):
        log.info(f"whatsapp status {status.get('id')} -> {status.get('status')}")

    # Always 2xx quickly so Meta does not re-deliver.
    return http.Response(status=200)


async def _already_seen(message_id: str) -> bool:
    # exists-then-store is not atomic; for at-most-once side effects under
    # concurrent re-delivery, use a store with an atomic set-if-absent backend.
    if await cache.exists(DEDUP_CACHE, message_id):
        return True
    await cache.store(DEDUP_CACHE, message_id, "1", ttl=DEDUP_TTL_SECS)
    return False


async def _handle_message(message: dict):
    sender = message.get("from")
    message_type = message.get("type")
    log.info(f"whatsapp inbound {message_type} from {sender}")
    # Example behaviour: echo text back. Replace with your own routing/bot logic.
    if message_type == "text":
        body = message.get("text", {}).get("body", "")
        await send_text(sender, f"echo: {body}")


# --------------------------------------------------------------------------
# Outbound (us -> Meta) over the pooled Graph client.
# --------------------------------------------------------------------------
async def send_text(to: str, text: str) -> int:
    """Send a plain text message. Returns the HTTP status from Graph."""
    return await _post_message(build_text_message(to, text))


async def send_template(to: str, template: str, lang: str = "en_US",
                        components: list | None = None) -> int:
    """Send a template message (required outside the 24h customer-service window)."""
    return await _post_message(build_template_message(to, template, lang, components))


async def mark_read(message_id: str) -> int:
    """Mark an inbound message as read (blue ticks)."""
    return await _post_message(
        {"messaging_product": "whatsapp", "status": "read", "message_id": message_id}
    )


async def _post_message(payload: dict) -> int:
    body = json.dumps(payload).encode()
    headers = {"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"}
    async with http.Client("graph") as client:
        response = await client.post(f"/{PHONE_NUMBER_ID}/messages", body=body, headers=headers)
    if response.status >= 300:
        log.error(f"whatsapp send failed {response.status}: {response.body!r}")
    return response.status


@http.on_startup
def _check_config():
    missing = [name for name, value in (
        ("WHATSAPP_TOKEN", TOKEN),
        ("WHATSAPP_APP_SECRET", APP_SECRET),
        ("WHATSAPP_VERIFY_TOKEN", VERIFY_TOKEN),
        ("WHATSAPP_PHONE_NUMBER_ID", PHONE_NUMBER_ID),
    ) if not value]
    if missing:
        log.warn(f"whatsapp: unset config, bridge will not work: {', '.join(missing)}")
