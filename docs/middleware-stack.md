# Middleware Stack

This document describes the request flow through the WSGI stack.

## Order (outermost to innermost)

1. Request logging middleware (debug logging)
2. CORS middleware
3. Static asset middleware
4. Setup wizard middleware
5. Home page middleware
6. Account API middleware
7. Login middleware
8. Registration middleware
9. Queue API middleware
10. Catalog API middleware
11. Auth middleware
12. Admin API middleware
13. PROPFIND cache middleware
14. WsgiDAV application

## Why this order matters

- CORS is placed near the outside so OPTIONS preflight requests are answered quickly.
- Setup and home middleware can provide browser UX without exposing WebDAV internals.
- Authentication runs before admin API authorization checks so role data is present.
- PROPFIND cache wraps WsgiDAV directly to reduce expensive Depth:infinity scans.

## Common troubleshooting map

- Request blocked as unauthorized: check auth middleware, then admin API role checks.
- Browser home page not shown: check setup and home middleware path handling.
- Stale library listing: check library watcher events and PROPFIND cache refresh.
- Missing static JS or CSS: check static middleware path and MIME mappings.
