# Trunk registry

A REST control-plane for **SIP trunks / peer gateways**. Because the HTTP addon
shares the process that routes SIP, you can expose a management API right next to
the router: operators (or the gateways themselves) **register** a trunk,
**heartbeat** to keep it live, **discover** the live set, and **deregister** on
the way out — the provisioning side of a SIP platform, over plain HTTP + JSON.

A "trunk" is a peer the platform can send calls to (a carrier SBC, a downstream
gateway): an id, a SIP URI, a transport, and whether it's enabled.

This mirrors
[`examples/trunk_registry.py`](https://github.com/siphon-project/siphon-http/blob/main/examples/trunk_registry.py).

## Script

```python
import json

from siphon import http, log

_TRUNKS: dict[str, dict] = {}
_TRANSPORTS = {"udp", "tcp", "tls"}
_HEARTBEAT_SECONDS = 30
_JSON = {"Content-Type": "application/json"}


def _error(status, message):
    return http.Response(status=status, headers=_JSON,
                         body=json.dumps({"status": status, "error": message}).encode())


@http.on_startup
async def announce():
    log.info("trunk registry up — PUT/PATCH/GET/DELETE /trunks/{id}, GET /trunks")


@http.route("/trunks/{id}", methods=["PUT", "PATCH", "GET", "DELETE"])
async def trunk(req):
    trunk_id = req.path_params["id"]

    if req.method == "PUT":                        # register or replace
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
        headers = dict(_JSON)
        if created:
            headers["Location"] = req.path            # 201 + Location on create
        return http.Response(status=201 if created else 200, headers=headers,
                             body=json.dumps(record).encode())

    if req.method == "PATCH":                      # heartbeat — refresh the lease
        if trunk_id not in _TRUNKS:
            return _error(404, "unknown trunk — re-register with PUT")
        return http.Response(status=204)

    if req.method == "DELETE":                      # deregister
        _TRUNKS.pop(trunk_id, None)
        return http.Response(status=204)

    record = _TRUNKS.get(trunk_id)                  # GET one
    if record is None:
        return _error(404, "unknown trunk")
    return http.Response(status=200, headers=_JSON, body=json.dumps(record).encode())


@http.route("/trunks", methods=["GET"])
async def discover(req):                            # discover, optionally filtered
    want_enabled = req.query_params.get("enabled")
    want_transport = req.query_params.get("transport")

    def matches(t):
        if want_enabled is not None and str(t.get("enabled", True)).lower() != want_enabled.lower():
            return False
        if want_transport is not None and t.get("transport") != want_transport:
            return False
        return True

    trunks = [t for t in _TRUNKS.values() if matches(t)]
    return http.Response(status=200, headers=_JSON,
                         body=json.dumps({"trunks": trunks}).encode())
```

## Listener

```yaml
# http.yaml
servers:
  - listen: "127.0.0.1:8080"
    max_body_bytes: 65536
```

## Try it

```bash
# register a carrier trunk — -i shows the Location header on first create
curl -isS -XPUT 127.0.0.1:8080/trunks/carrier-a \
     -H 'content-type: application/json' \
     -d '{"sip_uri": "sip:sbc-a.example.net:5060", "transport": "tls", "max_channels": 240}'

# heartbeat (keep the lease fresh), then read it back
curl -sS -XPATCH 127.0.0.1:8080/trunks/carrier-a
curl -sS         127.0.0.1:8080/trunks/carrier-a

# discover the live set, optionally filtered
curl -sS '127.0.0.1:8080/trunks?enabled=true&transport=tls'

# deregister
curl -sS -XDELETE 127.0.0.1:8080/trunks/carrier-a
```

## How it works

- **The register / heartbeat / discover / deregister pattern.** One resource keyed
  by trunk id — `PUT` to register or replace, `PATCH` to heartbeat, `GET` to read
  one, `DELETE` to deregister — plus a collection route to discover the set.
- **One route, several methods.** `/trunks/{id}` declares
  `methods=["PUT", "PATCH", "GET", "DELETE"]` and branches on `req.method`. See
  [Script API → `@http.route`](../script-api.md#httproutepath-methodsnone).
- **Status codes that mean something.** `201` + a `Location` header on first
  registration, `200` on replace, `204` on heartbeat/deregister, `404` on an
  unknown trunk so a stale peer knows to re-register.
- **Filtered discovery.** `GET /trunks?enabled=true&transport=tls` reads
  `req.query_params` to narrow the result.
- **A startup log line.** `@http.on_startup` runs once when the listener comes up.

!!! warning "State belongs in Rust or an external store"
    The `_TRUNKS` dict here is a **process-local** illustration only, so as
    written this is single-replica. A real deployment keeps the registry in a
    shared store / siphon primitive so every replica — and the SIP routing that
    reads it — sees the same trunks before scaling out. See
    [Script API → A note on state](../script-api.md#a-note-on-state).
