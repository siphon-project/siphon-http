"""
proxy.py — forward inbound requests to an upstream API over http.Client.

Shows the outbound side: a named, pooled client (configured under `clients.api`
in http.yaml) reused across requests, with a catch-all route that forwards the
remaining path to the upstream and relays the response.

    # http.yaml:
    # clients:
    #   api:
    #     base_url: "https://api.example.com"
    #     timeout_ms: 5000

    curl -sS localhost:8080/proxy/v1/status
"""

from siphon import http, log


@http.route("/proxy/{*rest}", methods=["GET"])
async def forward(req):
    path = "/" + req.path_params["rest"]
    async with http.Client("api") as c:
        upstream = await c.get(path)
    log.info(f"proxied {path} -> {upstream.status}")
    content_type = upstream.headers.get("content-type", "application/octet-stream")
    return http.Response(
        status=upstream.status,
        headers={"Content-Type": content_type},
        body=upstream.body,
    )
