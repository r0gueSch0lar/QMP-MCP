# HTTP transport is fail-closed and refuses to start without auth

When `QMP_MCP_TRANSPORT=http`, the server refuses to start unless an auth provider is
configured. The default provider is `APIKeyAuthProvider` (keys via `QMP_MCP_API_KEYS`,
header `X-API-Key`); `JWTAuthProvider` is opt-in via `QMP_MCP_AUTH=jwt`. The only way
to run HTTP unauthenticated is to set `QMP_MCP_ALLOW_INSECURE=true` explicitly, intended
for local development. Default bind is `127.0.0.1`; the Docker image sets `0.0.0.0`
out of necessity and relies on container isolation, required auth, and the framework's
DNS-rebinding/CORS protections.

We chose fail-closed because this endpoint can build and control VMs (and, via gated
features, touch the host); an accidentally-open unauthenticated port would be a
critical exposure. Refusing to start is safer than starting insecure.

Every such refusal MUST emit an actionable error stating the exact cause and the
remediation (e.g. "HTTP transport requires auth: set QMP_MCP_API_KEYS, or set
QMP_MCP_ALLOW_INSECURE=true to override for local dev"). Silent or vague startup
failures are treated as bugs.
