"""
rest_api.py — a tiny in-memory REST resource.

Demonstrates path params, methods, query params, and JSON bodies over a single
`@http.route` per path. State here is a process-local dict for illustration —
real services put state in Rust (a siphon primitive) or an external store, since
Python handlers may run across threads.

    curl -sS localhost:8080/items
    curl -sS -XPUT localhost:8080/items/42 -d '{"name": "widget"}'
    curl -sS localhost:8080/items/42
    curl -sS -XDELETE localhost:8080/items/42
"""

import json

from siphon import http

_ITEMS: dict[str, dict] = {}


@http.route("/items", methods=["GET"])
async def list_items(req):
    limit = int(req.query_params.get("limit", "100"))
    body = json.dumps(list(_ITEMS.values())[:limit]).encode()
    return http.Response(status=200, headers={"Content-Type": "application/json"}, body=body)


@http.route("/items/{id}", methods=["GET", "PUT", "DELETE"])
async def item(req):
    item_id = req.path_params["id"]

    if req.method == "PUT":
        try:
            doc = json.loads(req.body() or b"{}")
        except ValueError:
            return http.Response(status=400, body=b"invalid json\n")
        doc["id"] = item_id
        _ITEMS[item_id] = doc
        return http.Response(status=200, headers={"Content-Type": "application/json"},
                             body=json.dumps(doc).encode())

    if req.method == "DELETE":
        _ITEMS.pop(item_id, None)
        return http.Response(status=204)

    # GET
    doc = _ITEMS.get(item_id)
    if doc is None:
        return http.Response(status=404, body=b"not found\n")
    return http.Response(status=200, headers={"Content-Type": "application/json"},
                         body=json.dumps(doc).encode())
