"""
trunk_registry.py — a REST control-plane for SIP trunks / peer gateways.

A siphon binary routes SIP; this HTTP addon lets you expose a **management API
on the same process** so operators (or the gateways themselves) can register a
trunk, keep it alive with heartbeats, discover the live set, and deregister on
the way out — the provisioning side of a SIP platform, over plain HTTP + JSON.

A "trunk" here is a peer the platform can send calls to (a carrier SBC, a
downstream gateway): an id, a SIP URI, a transport, and whether it's enabled.
The registry is the source of truth your routing consults; this example keeps it
self-contained so it runs on its own.

It's the register / heartbeat / discover / deregister pattern (keyed by trunk
id, PUT/PATCH/GET/DELETE), and it exercises the parts of the addon the other
examples don't: reading request headers and setting a `Location` header.

── Usage (curl) ─────────────────────────────────────────────────────────────
    # register a carrier trunk — -i shows the Location header on first create
    curl -isS -XPUT localhost:8080/trunks/carrier-a \
         -H 'content-type: application/json' \
         -d '{"sip_uri": "sip:sbc-a.example.net:5060",
              "transport": "tls", "max_channels": 240}'

    # heartbeat (keep the lease fresh), then read it back
    curl -sS -XPATCH localhost:8080/trunks/carrier-a
    curl -sS         localhost:8080/trunks/carrier-a

    # discover the live set, optionally filtered
    curl -sS 'localhost:8080/trunks?enabled=true&transport=tls'

    # deregister
    curl -sS -XDELETE localhost:8080/trunks/carrier-a

── Running it ───────────────────────────────────────────────────────────────
Add an `http` listener to your siphon build's addon config (see the README and
`harness/http.yaml`), point the script path at this file, and start siphon.

State caveat: `_TRUNKS` is process-local (same as `rest_api.py`), so as written
this is single-replica. A real deployment keeps the registry in a shared store /
siphon primitive so every replica — and the SIP routing that reads it — sees the
same trunks before scaling out.

All hosts/URIs are synthetic (`example.net`, `127.0.0.1`).
"""

import json

from siphon import http, log

# trunk id -> trunk record. Process-local; replicate before scaling out
# (see the state caveat in the module docstring).
_TRUNKS: dict[str, dict] = {}

# Transports we accept on registration.
_TRANSPORTS = {"udp", "tcp", "tls"}

# Seconds we ask a registrant to wait between heartbeats.
_HEARTBEAT_SECONDS = 30

_JSON = {"Content-Type": "application/json"}


def _error(status, message):
    return http.Response(
        status=status,
        headers=_JSON,
        body=json.dumps({"status": status, "error": message}).encode(),
    )


@http.on_startup
async def announce():
    log.info("trunk registry up — PUT/PATCH/GET/DELETE /trunks/{id}, GET /trunks")


@http.route("/trunks/{id}", methods=["PUT", "PATCH", "GET", "DELETE"])
async def trunk(req):
    """Register / heartbeat / read / deregister a single trunk."""
    trunk_id = req.path_params["id"]

    if req.method == "PUT":
        # Register (or replace) the trunk. Idempotent on the id.
        try:
            record = json.loads(req.body() or b"{}")
        except ValueError:
            return _error(400, "malformed trunk record (invalid JSON)")
        if not record.get("sip_uri"):
            return _error(400, "sip_uri is required")
        transport = record.get("transport", "udp")
        if transport not in _TRANSPORTS:
            return _error(400, f"transport must be one of {sorted(_TRANSPORTS)}")

        record["id"] = trunk_id
        record["transport"] = transport
        record.setdefault("enabled", True)
        record["status"] = "up"
        record["heartbeat_seconds"] = _HEARTBEAT_SECONDS

        created = trunk_id not in _TRUNKS
        _TRUNKS[trunk_id] = record
        log.info(f"trunk {trunk_id} {'registered' if created else 'updated'} "
                 f"({record['sip_uri']} / {transport})")

        # 201 + Location on first registration, 200 on replace.
        headers = dict(_JSON)
        if created:
            headers["Location"] = req.path
        return http.Response(status=201 if created else 200, headers=headers,
                             body=json.dumps(record).encode())

    if req.method == "PATCH":
        # Heartbeat — refresh the lease. Unknown trunk → 404 so a stale peer
        # knows to re-register.
        if trunk_id not in _TRUNKS:
            return _error(404, "unknown trunk — re-register with PUT")
        return http.Response(status=204)

    if req.method == "DELETE":
        _TRUNKS.pop(trunk_id, None)
        log.info(f"trunk {trunk_id} deregistered")
        return http.Response(status=204)

    # GET — read one trunk.
    record = _TRUNKS.get(trunk_id)
    if record is None:
        return _error(404, "unknown trunk")
    return http.Response(status=200, headers=_JSON, body=json.dumps(record).encode())


@http.route("/trunks", methods=["GET"])
async def discover(req):
    """Discover the registered trunks, optionally filtered by ?enabled / ?transport."""
    want_enabled = req.query_params.get("enabled")   # "true" / "false" / None
    want_transport = req.query_params.get("transport")

    def matches(t):
        if want_enabled is not None and str(t.get("enabled", True)).lower() != want_enabled.lower():
            return False
        if want_transport is not None and t.get("transport") != want_transport:
            return False
        return True

    trunks = [t for t in _TRUNKS.values() if matches(t)]
    log.info(f"discover: {len(trunks)} trunk(s)")
    return http.Response(status=200, headers=_JSON,
                         body=json.dumps({"trunks": trunks}).encode())
