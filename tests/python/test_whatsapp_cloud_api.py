"""Tests for examples/whatsapp_cloud_api.py, driven through the siphon-sip SDK
mocks (siphon_sdk.http_testing.HttpTestHarness + the cache mock).

Covers the two halves and the security-critical bits:
  * pure helpers: raw-body HMAC signature verification, message-body builders,
    and webhook payload extraction;
  * the GET verify handshake (echoes hub.challenge only on a matching token);
  * the POST webhook rejecting a bad/absent X-Hub-Signature-256 and accepting a
    valid one, deduping repeated message ids, and driving the outbound Graph POST.
"""
import hashlib
import hmac
import importlib.util
import json
import os
import pathlib

import pytest

from siphon_sdk import mock_module

mock_module.install()

from siphon_sdk.http_testing import HttpTestHarness  # noqa: E402

EXAMPLE_PATH = (
    pathlib.Path(__file__).resolve().parent.parent.parent / "examples" / "whatsapp_cloud_api.py"
)
APP_SECRET = "test-app-secret"
VERIFY_TOKEN = "test-verify-token"
PHONE_NUMBER_ID = "123456"


def _import_example():
    spec = importlib.util.spec_from_file_location("whatsapp_cloud_api_example", EXAMPLE_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _sign(body: bytes) -> str:
    return "sha256=" + hmac.new(APP_SECRET.encode(), body, hashlib.sha256).hexdigest()


def _message_payload(message_id="wamid.1", sender="15551234567", text="hi"):
    return {"entry": [{"changes": [{"value": {"messages": [
        {"id": message_id, "from": sender, "type": "text", "text": {"body": text}},
    ]}}]}]}


def _status_payload(message_id="wamid.9", status="delivered"):
    return {"entry": [{"changes": [{"value": {"statuses": [
        {"id": message_id, "status": status},
    ]}}]}]}


# --------------------------------------------------------------------------
# Pure helpers (no server).
# --------------------------------------------------------------------------
class TestPureHelpers:
    module = _import_example()

    def test_signature_valid(self):
        body = b'{"a":1}'
        assert self.module.verify_signature(APP_SECRET, body, _sign(body)) is True

    def test_signature_wrong_secret(self):
        body = b'{"a":1}'
        forged = "sha256=" + hmac.new(b"wrong", body, hashlib.sha256).hexdigest()
        assert self.module.verify_signature(APP_SECRET, body, forged) is False

    def test_signature_missing_or_malformed(self):
        assert self.module.verify_signature(APP_SECRET, b"x", None) is False
        assert self.module.verify_signature(APP_SECRET, b"x", "deadbeef") is False

    def test_build_text_message(self):
        message = self.module.build_text_message("15551234567", "hello")
        assert message["messaging_product"] == "whatsapp"
        assert message["to"] == "15551234567"
        assert message["type"] == "text"
        assert message["text"]["body"] == "hello"

    def test_build_template_message(self):
        message = self.module.build_template_message("15551234567", "hello_world", lang="en_GB")
        assert message["type"] == "template"
        assert message["template"]["name"] == "hello_world"
        assert message["template"]["language"]["code"] == "en_GB"

    def test_iter_inbound_messages_and_statuses(self):
        assert [m["id"] for m in self.module.iter_inbound_messages(_message_payload())] == ["wamid.1"]
        assert [s["status"] for s in self.module.iter_status_updates(_status_payload())] == ["delivered"]
        # A status-only payload yields no inbound messages, and vice versa.
        assert list(self.module.iter_inbound_messages(_status_payload())) == []
        assert list(self.module.iter_status_updates(_message_payload())) == []


# --------------------------------------------------------------------------
# Webhook routes + outbound Graph call.
# --------------------------------------------------------------------------
class TestWebhook:
    @pytest.fixture
    def harness(self):
        os.environ["WHATSAPP_APP_SECRET"] = APP_SECRET
        os.environ["WHATSAPP_VERIFY_TOKEN"] = VERIFY_TOKEN
        os.environ["WHATSAPP_TOKEN"] = "test-token"
        os.environ["WHATSAPP_PHONE_NUMBER_ID"] = PHONE_NUMBER_ID
        h = HttpTestHarness()
        h.load_script(str(EXAMPLE_PATH))
        h.cache.set_data("wa_dedup", {})   # the configured dedup cache
        yield h
        h.reset()
        h.close()

    def test_verify_handshake_echoes_challenge(self, harness):
        response = harness.request("GET", "/whatsapp/webhook", query_params={
            "hub.mode": "subscribe",
            "hub.verify_token": VERIFY_TOKEN,
            "hub.challenge": "chal-42",
        })
        assert response.status == 200
        assert response.body == b"chal-42"

    def test_verify_handshake_wrong_token_forbidden(self, harness):
        response = harness.request("GET", "/whatsapp/webhook", query_params={
            "hub.mode": "subscribe",
            "hub.verify_token": "nope",
            "hub.challenge": "chal-42",
        })
        assert response.status == 403

    def test_post_rejects_bad_signature(self, harness):
        body = json.dumps(_message_payload()).encode()
        response = harness.request(
            "POST", "/whatsapp/webhook",
            headers={"x-hub-signature-256": "sha256=deadbeef"}, body=body,
        )
        assert response.status == 403
        assert harness.sent_requests == []   # never processed

    def test_post_accepts_valid_signature_and_sends_reply(self, harness):
        body = json.dumps(_message_payload(text="ping")).encode()
        response = harness.request(
            "POST", "/whatsapp/webhook",
            headers={"x-hub-signature-256": _sign(body)}, body=body,
        )
        assert response.status == 200
        assert len(harness.sent_requests) == 1
        sent = harness.sent_requests[0]
        assert sent["method"] == "POST"
        assert sent["path"] == f"/{PHONE_NUMBER_ID}/messages"
        assert sent["headers"]["Authorization"] == "Bearer test-token"
        assert json.loads(sent["body"])["text"]["body"] == "echo: ping"

    def test_post_dedups_repeated_message_id(self, harness):
        body = json.dumps(_message_payload(message_id="wamid.dup")).encode()
        headers = {"x-hub-signature-256": _sign(body)}
        harness.request("POST", "/whatsapp/webhook", headers=headers, body=body)
        harness.request("POST", "/whatsapp/webhook", headers=headers, body=body)
        # Second delivery is deduped, so only one outbound reply.
        assert len(harness.sent_requests) == 1

    def test_post_status_only_no_outbound(self, harness):
        body = json.dumps(_status_payload()).encode()
        response = harness.request(
            "POST", "/whatsapp/webhook",
            headers={"x-hub-signature-256": _sign(body)}, body=body,
        )
        assert response.status == 200
        assert harness.sent_requests == []
