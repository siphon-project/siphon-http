"""
nrf_5g.py — a miniature 5G Core NF Repository Function (NRF) over SBI.

The NRF is the service registry of the 5G core: every Network Function (NF)
registers its profile, sends periodic heartbeats, discovers peers, and
deregisters on shutdown — all over the 3GPP Service Based Interface, which is
plain HTTP/2 + JSON (TS 29.510 / TS 29.500). That maps cleanly onto siphon-http:
a resource keyed by nfInstanceId, with PUT/PATCH/GET/DELETE plus query- and
header-driven discovery. It also exercises the parts of the addon the other
examples don't: reading request headers and setting response headers.

── Conformance scope ────────────────────────────────────────────────────────
Illustrative, not a conformant NF. It DOES honour the SBI mechanics that shape
the HTTP: mandatory discovery params, the `3gpp-Sbi-Discovery-*` header form used
for delegated discovery, the `Location` header on registration, `heartBeatTimer`
in the registration response, and `application/problem+json` errors. A real NRF
additionally needs OAuth2 access tokens (`Authorization: Bearer …`, issued by the
NRF's own Nnrf_AccessToken service), mutual-TLS between NFs, the wider `3gpp-Sbi-*`
header set (`3gpp-Sbi-Message-Priority`, `3gpp-Sbi-Sender-Timestamp`, `User-Agent`
= requester NF type, …), heartbeat as a real JSON Patch, and a replicated store.

── Usage (curl) ─────────────────────────────────────────────────────────────
    # register a UDM instance (nfInstanceId is a UUID) — -i shows the Location header
    curl -isS -XPUT localhost:8080/nnrf-nfm/v1/nf-instances/6faf1bbc-6e4a-4454-a507-a14ef8e1bc5c \
         -H 'content-type: application/json' \
         -d '{"nfType": "UDM", "nfStatus": "REGISTERED",
              "ipv4Addresses": ["127.0.0.1"],
              "nfServices": [{"serviceName": "nudm-sdm"}]}'

    # discovery — both target-nf-type and requester-nf-type are mandatory
    curl -sS 'localhost:8080/nnrf-disc/v1/nf-instances?target-nf-type=UDM&requester-nf-type=AMF'

    # same discovery via delegated-discovery headers (what an SCP would send)
    curl -sS localhost:8080/nnrf-disc/v1/nf-instances \
         -H '3gpp-Sbi-Discovery-target-nf-type: UDM' \
         -H '3gpp-Sbi-Discovery-requester-nf-type: AMF'

    # heartbeat, then deregister
    curl -sS -XPATCH  localhost:8080/nnrf-nfm/v1/nf-instances/6faf1bbc-6e4a-4454-a507-a14ef8e1bc5c
    curl -sS -XDELETE localhost:8080/nnrf-nfm/v1/nf-instances/6faf1bbc-6e4a-4454-a507-a14ef8e1bc5c

── Running it ───────────────────────────────────────────────────────────────
Add an `http` listener to your siphon build's addon config (see the README and
`harness/http.yaml`), point the script path at this file, and start siphon. SBI
mandates HTTP/2: terminate h2 via ALPN on a TLS listener, or use cleartext h2c
(`http2_prior_knowledge`). Everything below :8080 in the examples is plain HTTP
for curl convenience.

Deploying and scaling this as a 5G CNF on Kubernetes — free-threaded CPython for
per-core throughput, replicas + HPA, and the HTTP/2 load-balancing caveat (the
SCP's job in a real 5GC) — is covered in the README ("Deploying on Kubernetes").
State caveat: `_PROFILES` is process-local (same as `rest_api.py`), so as written
this example is single-replica — a real NRF backs the registry with a shared /
replicated store before scaling out.
"""

import json

from siphon import http, log

# nfInstanceId -> NF profile (TS 29.510 §6.1.6.2.2). Process-local; a real NRF
# replicates this store (see the state caveat in the module docstring).
_PROFILES: dict[str, dict] = {}

# Seconds the NRF tells a registering NF to wait between heartbeats.
_HEARTBEAT_TIMER = 10

_JSON = {"Content-Type": "application/json"}


def _problem(status, title, cause=None):
    # RFC 7807 ProblemDetails (TS 29.571 §5.2.4) — SBI's error envelope.
    body = {"status": status, "title": title}
    if cause is not None:
        body["cause"] = cause
    return http.Response(
        status=status,
        headers={"Content-Type": "application/problem+json"},
        body=json.dumps(body).encode(),
    )


@http.on_startup
async def announce():
    log.info("NRF up — Nnrf_NFManagement + Nnrf_NFDiscovery on SBI")


@http.route(
    "/nnrf-nfm/v1/nf-instances/{nfInstanceId}",
    methods=["PUT", "PATCH", "GET", "DELETE"],
)
async def nf_instance(req):
    """Nnrf_NFManagement — register / heartbeat / read / deregister one NF."""
    nf_id = req.path_params["nfInstanceId"]

    if req.method == "PUT":
        # Register (or replace) the NF profile.
        try:
            profile = json.loads(req.body() or b"{}")
        except ValueError:
            return _problem(400, "malformed NF profile", cause="MANDATORY_IE_INCORRECT")
        profile["nfInstanceId"] = nf_id
        profile.setdefault("nfStatus", "REGISTERED")
        # The NRF dictates the heartbeat cadence back to the NF.
        profile["heartBeatTimer"] = _HEARTBEAT_TIMER
        created = nf_id not in _PROFILES
        _PROFILES[nf_id] = profile
        log.info(f"NF {profile.get('nfType', '?')} {nf_id} "
                 f"{'registered' if created else 'updated'}")
        # 201 on first registration (with Location), 200 on replace
        # (TS 29.510 §5.2.2.2.2).
        headers = dict(_JSON)
        if created:
            headers["Location"] = req.path
        return http.Response(status=201 if created else 200, headers=headers,
                             body=json.dumps(profile).encode())

    if req.method == "PATCH":
        # Heartbeat / partial update. A real NF sends a JSON Patch touching
        # nfStatus every heartBeatTimer seconds; we accept an empty PATCH here.
        if nf_id not in _PROFILES:
            return _problem(404, "unknown NF instance", cause="NF_NOT_FOUND")
        return http.Response(status=204)

    if req.method == "DELETE":
        _PROFILES.pop(nf_id, None)
        log.info(f"NF {nf_id} deregistered")
        return http.Response(status=204)

    # GET — read back one profile.
    profile = _PROFILES.get(nf_id)
    if profile is None:
        return _problem(404, "unknown NF instance", cause="NF_NOT_FOUND")
    return http.Response(status=200, headers=_JSON, body=json.dumps(profile).encode())


def _disc_param(req, name):
    """Read a discovery parameter from the query string, falling back to the
    `3gpp-Sbi-Discovery-<name>` header used for delegated discovery (via an SCP).
    Query string wins when both are present (TS 29.500 §5.2.3.2.7)."""
    return req.query_params.get(name) or req.header(f"3gpp-Sbi-Discovery-{name}")


@http.route("/nnrf-disc/v1/nf-instances", methods=["GET"])
async def discover(req):
    """Nnrf_NFDiscovery — find registered NFs by type and/or service name."""
    target_type = _disc_param(req, "target-nf-type")
    requester_type = _disc_param(req, "requester-nf-type")
    # Both are mandatory (TS 29.510 §6.2.3.2.3.1).
    if not target_type or not requester_type:
        return _problem(400, "target-nf-type and requester-nf-type are required",
                        cause="MANDATORY_QUERY_PARAM_INCORRECT")
    want_service = _disc_param(req, "service-names")

    def matches(p):
        if p.get("nfStatus") != "REGISTERED":
            return False
        if p.get("nfType") != target_type:
            return False
        if want_service:
            names = {s.get("serviceName") for s in p.get("nfServices", [])}
            if want_service not in names:
                return False
        return True

    instances = [p for p in _PROFILES.values() if matches(p)]
    log.info(f"discovery {requester_type} -> {target_type}: {len(instances)} hit(s)")
    # SearchResult object (TS 29.510 §6.2.6.2.2).
    result = {"validityPeriod": 3600, "nfInstances": instances}
    return http.Response(status=200, headers=_JSON, body=json.dumps(result).encode())
