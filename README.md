# qmp-mcp

A secure [Model Context Protocol](https://modelcontextprotocol.io) server that lets an
AI agent build, launch, drive, and tear down a single QEMU virtual machine through
QEMU's [QMP](https://www.qemu.org/docs/master/interop/qmp-spec.html) API.

Built on [`mcp-framework`](https://mcp-framework.com). See [`CONTEXT.md`](./CONTEXT.md)
for the domain glossary and [`docs/adr/`](./docs/adr) for the architectural decisions.

> **Status:** walking skeleton. This build ships the stdio transport and a single
> read-only `get_instance` tool. VM lifecycle, the QMP surface, the HTTP transport,
> and Docker packaging arrive in subsequent slices.

## Run (stdio)

```bash
npx -y qmp-mcp
```

Or as an MCP client config:

```json
{
  "command": "npx",
  "args": ["-y", "qmp-mcp"],
  "env": {}
}
```

### Tools

- **`get_instance`** — returns the current Instance and its lifecycle state
  (`NONE` when no Instance is running).

## Configuration

Configured entirely through `QMP_MCP_*` environment variables. Invalid values fail
closed at startup with a message naming the variable and its allowed values.

| Variable | Default | Purpose |
| --- | --- | --- |
| `QMP_MCP_TRANSPORT` | `stdio` | Transport: `stdio` \| `http` \| `both` (only `stdio` is available in this build). |
| `QMP_MCP_LOG_LEVEL` | `info` | Logger verbosity: `debug` \| `info` \| `warning` \| `error`. |

## Development

```bash
npm install
npm run build      # tsc -> dist/
npm run typecheck  # tsc --noEmit
npm run test       # vitest
npm run lint       # biome
npm run format     # biome --write
```

Requires Node.js >= 20.
