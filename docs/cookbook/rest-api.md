# REST API

A tiny in-memory REST resource that shows path params, several methods on one
route, query params, and JSON bodies.

This mirrors
[`examples/rest_api.py`](https://github.com/siphon-project/siphon-http/blob/main/examples/rest_api.py).

## Script

```python
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
curl -sS 127.0.0.1:8080/items
curl -sS -XPUT 127.0.0.1:8080/items/42 -d '{"name": "widget"}'
curl -sS 127.0.0.1:8080/items/42
curl -sS -XDELETE 127.0.0.1:8080/items/42
```

## How it works

- **One route, several methods.** `/items/{id}` declares
  `methods=["GET", "PUT", "DELETE"]` and branches on `req.method`. See
  [Script API → `@http.route`](../script-api.md#httproutepath-methodsnone).
- **Path params.** `{id}` is extracted, URL-decoded, and read from
  `req.path_params["id"]`.
- **Query params.** `list_items` reads `?limit=` from `req.query_params`.
- **Bodies.** `req.body()` returns bytes; the handler parses/serialises JSON in
  Python.

!!! warning "State belongs in Rust or an external store"
    The `_ITEMS` dict here is a **process-local** illustration only. Python
    handlers may run across threads on free-threaded CPython, so real services
    keep mutable state in Rust (a siphon primitive) or an external store — not a
    module global. See [Script API → A note on state](../script-api.md#a-note-on-state).
