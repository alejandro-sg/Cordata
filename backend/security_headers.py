"""Response headers hardening for every route this backend serves.

Deliberately does NOT set X-Frame-Options / a frame-ancestors CSP here —
unlike the frontend, this backend has no page of its own a third-party site
could clickjack (every JSON/API response is meaningless framed either way),
and one of its routes (sandbox_functions_api.py's /sandbox/*) exists
specifically to be loaded into this app's own
<iframe sandbox="allow-scripts">, proxied through the frontend. A frame
header set here would do nothing for the human anyway (the frontend already
carries the real one — see frontend/next.config.js), and get this backend's
own iframe-serving routes wrong the same way an unscoped rule would there.

X-Content-Type-Options and Referrer-Policy carry no such caveat — safe and
meaningful on every response, regardless of what serves it or what embeds
it — so those two apply here unconditionally.
"""

from starlette.middleware.base import BaseHTTPMiddleware
from starlette.requests import Request
from starlette.responses import Response


class SecurityHeadersMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next) -> Response:
        response = await call_next(request)
        response.headers["X-Content-Type-Options"] = "nosniff"
        response.headers["Referrer-Policy"] = "no-referrer"
        return response


def register_security_headers(app) -> None:
    app.add_middleware(SecurityHeadersMiddleware)
