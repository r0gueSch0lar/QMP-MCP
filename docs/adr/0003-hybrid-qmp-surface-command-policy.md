# Hybrid QMP surface governed by a default-safe Command Policy

The agent drives a running Instance through two layers: a curated set of first-class
MCP tools for common operations (status, power, pause/resume, reset, screendump,
block query, …), plus a single generic `qmp_execute` tool. The generic tool checks
each command name against a **Command Policy** — a default-safe allowlist, with a hard
denylist for dangerous commands.

We chose the hybrid over generic-only or curated-only because curated tools give the
LLM clear, well-described affordances for the common case, while the gated generic
tool preserves QMP's full breadth without exposing the dangerous commands by default.

The denylist (`human-monitor-command`, `migrate`/`migrate-incoming`,
`dump-guest-memory`, `pmemsave`, `memsave`, `object-add`, file-backed `blockdev-add`/
`device_add`, `getfd`/`add-fd`) is deliberate: each can exfiltrate guest/host memory,
read/write host files, open host resources, or — in the case of
`human-monitor-command` — run arbitrary HMP and bypass every other QMP control.
Do not relax it without understanding that each enables host-level access.
