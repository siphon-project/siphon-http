"""
siphon.http -- HTTP server + client namespace.

Imported by user scripts as `from siphon import http`. Decorators register
handlers in `_siphon_registry`; the Rust side (siphon-http) reads them after
script load and dispatches HTTP requests from real listeners.

Handler kinds:
  - "http.route"      -- fires for routes matched by path + method
  - "http.middleware" -- a request guard run (in registration order) before
                         the route handler; return a Response to short-circuit,
                         or None to continue
  - "http.startup"    -- runs once, to completion, before any listener accepts

Handlers may be sync or async; async is preferred for anything that awaits I/O
(an outbound http.Client call, a cache lookup, ...).

Note on import timing: this module is constructed by siphon-http's namespace
closure at engine init, which runs **before** siphon finishes wiring up
`_siphon_registry`. So we don't import the registry at module load -- each
decorator does a lazy import on first call, when the user script is being
evaluated and `_siphon_registry` is up.
"""

import asyncio


def _registry():
    # Lazy import -- see module docstring on import timing.
    import _siphon_registry as _r

    return _r


def route(path, methods=None):
    """
    Register a handler for an HTTP route.

    `path` is a matchit-compatible pattern: `/users/{id}`, `/static/{*rest}`,
    etc. Path params are extracted and exposed as `req.path_params`.

    `methods` is a list of HTTP method strings; default is `["GET"]`.

    The handler receives a single `Request` argument and returns a `Response`.
    Returning anything else (including None) is a script error and produces a
    500.
    """
    if methods is None:
        methods = ["GET"]
    methods = [m.upper() for m in methods]

    def decorator(fn):
        _registry().register(
            "http.route",
            None,
            fn,
            asyncio.iscoroutinefunction(fn),
            {"path": path, "methods": methods},
        )
        return fn

    return decorator


def middleware(fn):
    """
    Register a request-guard middleware.

    Middlewares run in registration order before the matched route handler,
    each receiving the `Request`. Return a `Response` to short-circuit (the
    route handler is not called); return `None` to continue to the next
    middleware (or the route handler). Typical uses: authentication, IP
    allow-listing, rate limiting, request logging.

    (Response-rewriting middleware -- the wrap-around `(req, call_next)` form --
    is a roadmap item; for now do post-processing inside the route handler.)
    """
    _registry().register(
        "http.middleware", None, fn, asyncio.iscoroutinefunction(fn), {}
    )
    return fn


def on_startup(fn):
    """
    Register a startup hook. Runs once, to completion, after the script loads
    and before any listener accepts. Useful for preloading data, warming
    caches, opening shared clients, etc.
    """
    _registry().register(
        "http.startup", None, fn, asyncio.iscoroutinefunction(fn), {}
    )
    return fn


# -- Helpers exposed to scripts ------------------------------------------
# These prototypes are replaced by the real Rust pyclasses when the module is
# loaded inside a siphon-http-enabled binary (see siphon-http's namespace
# hook). They exist here for IDE type hinting and as a clear error when the
# module is imported outside such a binary.


class Request:
    """An inbound HTTP request. Provided as the only arg to handlers.

    Attributes:
        method:       "GET" / "PUT" / ...
        path:         request path
        path_params:  dict[str, str] -- extracted from the matched route
        query_params: dict[str, str]
        headers:      lowercase-keyed dict
        client:       remote socket address as "ip:port"

    Methods:
        req.body() -> bytes              (body is buffered)
        req.header(name) -> str | None   (case-insensitive)
    """

    method: str
    path: str
    path_params: dict
    query_params: dict
    headers: dict
    client: str

    def body(self) -> bytes:
        raise NotImplementedError("overridden by siphon-http at install time")

    def header(self, name: str):
        raise NotImplementedError("overridden by siphon-http at install time")


class Response:
    """An outbound HTTP response.

    Construct with: `Response(status=200, headers={...}, body=b"...")`.
    Body accepts bytes or str (UTF-8 encoded).
    """

    status: int
    headers: dict

    def __init__(self, status=200, headers=None, body=None):
        raise NotImplementedError("provided by Rust side at runtime")

    @property
    def body(self) -> bytes:
        raise NotImplementedError("provided by Rust side at runtime")

    def raise_for_status(self):
        raise NotImplementedError("provided by Rust side at runtime")


class Client:
    """An outbound HTTP client wrapping a pooled reqwest::Client.

    Two construction modes:

      http.Client("api")              # named, looks up clients.api in config
      http.Client(base_url="https://...", verify="/path/ca.crt",
                  cert=("/path/c.crt", "/path/c.key"))

    All methods are coroutines returning a Response:

      async with http.Client("api") as c:
          resp = await c.get("/v1/things")
          resp.raise_for_status()
    """

    def __init__(self, name=None, *, base_url=None, verify=None, cert=None, timeout_ms=None,
                 http2_prior_knowledge=False):
        raise NotImplementedError("provided by Rust side at runtime")

    async def get(self, path, *, headers=None) -> "Response":
        raise NotImplementedError

    async def put(self, path, *, body=None, headers=None) -> "Response":
        raise NotImplementedError

    async def post(self, path, *, body=None, headers=None) -> "Response":
        raise NotImplementedError

    async def patch(self, path, *, body=None, headers=None) -> "Response":
        raise NotImplementedError

    async def delete(self, path, *, headers=None) -> "Response":
        raise NotImplementedError

    async def __aenter__(self):
        raise NotImplementedError

    async def __aexit__(self, exc_type, exc, tb):
        raise NotImplementedError
