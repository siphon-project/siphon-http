"""
bench_echo.py — zero-overhead route for load measurement.

A single GET route that replies 200 with a tiny body and does NO per-request
logging: at high request rates a log line per request is the bottleneck, not
siphon-http's dispatch, so this strips it to expose the real ceiling. Driven by
harness/run.sh (drive) via harness/siphon.bench.yaml.
"""

from siphon import http


@http.route("/", methods=["GET"])
async def echo(req):
    return http.Response(status=200, body=b"ok")
