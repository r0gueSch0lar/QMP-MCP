/**
 * The Command Policy engine (ADR-0003): decides whether the generic
 * {@link QmpExecuteTool `qmp_execute`} tool may run a given QMP command name.
 *
 * Three layers, in strict precedence:
 *
 *   1. An immutable HARD DENYLIST — {@link HARD_DENYLIST}. A command on it is
 *      ALWAYS refused and can NEVER be re-enabled by env, a policy file, or any
 *      allowlist. This is the security boundary; it is defined exactly once here
 *      as the single source of truth.
 *   2. A curated default-safe allowlist — {@link DEFAULT_ALLOWLIST} — of
 *      read/query and a few safe control commands.
 *   3. Operator overrides — `QMP_MCP_ALLOW`/`QMP_MCP_DENY` and an optional YAML
 *      policy file (`QMP_MCP_POLICY_FILE`) — that may ADD to or REMOVE from the
 *      allowlist. They can never resurrect a hard-denied command.
 *
 * The decision is a pure function ({@link decideCommand}: resolved policy +
 * command name -> verdict). Resolving the policy from env + file
 * ({@link resolveCommandPolicy}) is the only part that touches the environment or
 * the filesystem, and it fails closed with an actionable error.
 */

import { readFileSync } from 'node:fs';
import { parse as parseYaml } from 'yaml';
import { z } from 'zod';
import { logger } from '../logger.js';

/**
 * Raised when the Command Policy cannot be resolved — a missing or unreadable
 * `QMP_MCP_POLICY_FILE`, malformed YAML, or a file whose shape is not
 * `{ allow?: string[], deny?: string[] }`. The message always names
 * `QMP_MCP_POLICY_FILE` and the remediation; the server fails closed rather than
 * starting with a half-understood policy.
 */
export class PolicyError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'PolicyError';
  }
}

/**
 * Raised when the Command Policy refuses a command requested through
 * `qmp_execute`. Distinct from {@link PolicyError} (which is about loading the
 * policy): this is a per-call denial. `hardDenied` records whether the refusal
 * came from the immutable hard denylist, so callers can surface that it can
 * never be enabled.
 */
export class CommandPolicyError extends Error {
  /** True iff the command is on the immutable {@link HARD_DENYLIST}. */
  readonly hardDenied: boolean;
  constructor(message: string, hardDenied: boolean) {
    super(message);
    this.name = 'CommandPolicyError';
    this.hardDenied = hardDenied;
  }
}

/**
 * Normalise a QMP command name for policy matching: trim surrounding whitespace
 * and lower-case it. This is what stops denylist evasion — ` migrate `,
 * `MIGRATE`, and `Human-Monitor-Command` all normalise onto their canonical
 * entry, so neither a stray space nor a case flip can slip a dangerous command
 * past {@link HARD_DENYLIST}. The denylist and allowlist constants are stored in
 * this normalised form, and every lookup goes through here.
 *
 * Fail-closed on a non-string input: rather than throwing a `TypeError` (this is
 * a pure, reused helper), return the empty string, which matches no allowlist or
 * denylist entry and so resolves to a default-deny at decision time.
 */
export function normalizeCommandName(raw: string): string {
  if (typeof raw !== 'string') return '';
  return raw.trim().toLowerCase();
}

/**
 * The immutable hard denylist — the single source of truth for commands that are
 * NEVER permitted, regardless of any allow override. Each entry can exfiltrate
 * guest/host memory, read or write arbitrary host files, open host resources, or
 * (`human-monitor-command`) run arbitrary HMP and bypass every other QMP control:
 *
 *   - `human-monitor-command` — runs arbitrary Human Monitor commands, bypassing
 *     the entire QMP policy surface.
 *   - migration family — streams the full VM (RAM + device state) to an arbitrary
 *     fd, host, or network target, pulls a foreign state in, or steers an
 *     in-flight migration (postcopy/recovery/pause/continue).
 *   - Xen device-state save/load — serialise/deserialise device state to/from a
 *     host file descriptor.
 *   - guest-memory dumps — copy guest/host memory to a host file.
 *   - object/backend hotplug — wire up host-backed objects, block backends,
 *     devices, netdevs, or chardevs that reference arbitrary host paths/sockets.
 *   - `qom-set` — write arbitrary QOM object properties (can repoint host-backed
 *     properties).
 *   - fd passing — hand the QEMU process host file descriptors.
 *   - block backup/mirror/export/create — copy a guest disk to, or expose it at,
 *     an arbitrary host file or network endpoint.
 *   - block jobs / snapshots / resize — mutate or grow a guest disk, or write a
 *     snapshot image to an arbitrary host path (`block-commit`, `block-stream`,
 *     `block_resize`, the `blockdev-snapshot*` family).
 *
 * Defined in normalised form (see {@link normalizeCommandName}).
 */
export const HARD_DENYLIST: ReadonlySet<string> = new Set([
  // Arbitrary HMP — bypasses every other control.
  'human-monitor-command',
  // Migration: exfiltrate/inject full VM state, or steer an in-flight migration
  // (incl. postcopy / recovery) to/from an arbitrary host or network target.
  'migrate',
  'migrate-incoming',
  'migrate-set-parameters',
  'migrate-set-capabilities',
  'migrate-recover',
  'migrate-continue',
  'migrate-pause',
  'migrate-start-postcopy',
  // Xen device-state save/load: serialise/deserialise full device state to/from
  // a host file descriptor.
  'xen-save-devices-state',
  'xen-load-devices-state',
  // Memory exfiltration to a host file.
  'dump-guest-memory',
  'pmemsave',
  'memsave',
  // Host-backed object/device/backend hotplug.
  'object-add',
  'blockdev-add',
  'device_add',
  'netdev_add',
  'chardev-add',
  'chardev-change',
  // Serial Port console input (ADR-0015): `ringbuf-write` types into the guest console —
  // keyboard-equivalent control, not a report. The ONLY sanctioned console-write path is the
  // dedicated write_serial tool behind its operator opt-in gate; dedicated tools bypass this
  // policy, so denying it here never blocks that tool. Keeps console-write a single gate.
  'ringbuf-write',
  // Arbitrary QOM property writes — can repoint host-backed object properties.
  'qom-set',
  // Passing host file descriptors into QEMU.
  'getfd',
  'add-fd',
  // Block backup/mirror/export/create: copy or expose a guest disk to the host
  // filesystem or the network.
  'drive-backup',
  'blockdev-backup',
  'drive-mirror',
  'blockdev-mirror',
  'blockdev-create',
  'block-export-add',
  'nbd-server-start',
  'nbd-server-add',
  // Block jobs / snapshots / resize: mutate or grow a guest disk, or create a
  // snapshot image at an arbitrary host path.
  'block-commit',
  'block-stream',
  'block_resize',
  'blockdev-snapshot',
  'blockdev-snapshot-sync',
  'blockdev-snapshot-internal-sync',
]);

/**
 * The curated default-safe allowlist: read/query commands plus a few safe
 * control commands (the same control surface already exposed as first-class
 * tools). Everything here is either read-only or a non-exfiltrating control
 * action. Defined in normalised form; disjoint from {@link HARD_DENYLIST} (an
 * invariant asserted in the tests).
 */
export const DEFAULT_ALLOWLIST: ReadonlySet<string> = new Set([
  // Run-state / identity (read-only).
  'query-status',
  'query-version',
  'query-name',
  'query-uuid',
  // `query-kvm` is deprecated since QEMU 11.0 in favour of `query-accelerators`
  // (new in 11.0). Both are allowlisted so `qmp_execute` works across the QEMU
  // version range a mixed bare-metal/container fleet spans (issue #31).
  'query-kvm',
  'query-accelerators',
  'query-target',
  // CPUs / topology (read-only).
  'query-cpus-fast',
  'query-cpu-definitions',
  'query-hotpluggable-cpus',
  // Memory / balloon (read-only).
  'query-memory-size-summary',
  'query-memdev',
  'query-balloon',
  // Block / storage (read-only).
  'query-block',
  'query-blockstats',
  'query-block-jobs',
  'query-named-block-nodes',
  // Devices / buses / IO (read-only).
  'query-pci',
  'query-chardev',
  'query-iothreads',
  // Machine / capabilities / introspection (read-only).
  'query-machines',
  'query-commands',
  // `query-events` intentionally omitted: QEMU removed it in 6.0 (superseded by
  // `query-qmp-schema`), so it could never succeed on any supported QEMU (issue #31).
  'query-qmp-schema',
  // Display / input (read-only).
  'query-vnc',
  'query-spice',
  'query-mice',
  // Safe control (already exposed as curated tools).
  'stop',
  'cont',
  'system_reset',
  'system_powerdown',
  // NOTE: `screendump` is intentionally NOT here. It writes an arbitrary
  // host file at the path in its `arguments`, and this policy gates command
  // NAMES, not arguments — so allowing it here would let `qmp_execute` write
  // any host file (e.g. ~/.ssh/authorized_keys). Screenshots are exposed only
  // through the dedicated `screendump` tool, which server-controls the path
  // (see Orchestrator.screendump) and never routes through this policy (#11).
]);

/**
 * A resolved Command Policy: the effective allow and deny sets, both normalised.
 * `allow` is the default allowlist plus any allow overrides; `deny` is the union
 * of the override deny lists. The hard denylist is intentionally NOT stored here
 * — it lives only in {@link HARD_DENYLIST} so a resolved policy can never weaken
 * it. Consumed by the pure {@link decideCommand}.
 */
export interface ResolvedPolicy {
  /** Normalised allowlist: defaults ∪ override allow lists. */
  readonly allow: ReadonlySet<string>;
  /** Normalised deny overrides (env + file). Removes from the allowlist. */
  readonly deny: ReadonlySet<string>;
}

/** A Command Policy verdict for a single command name. */
export type CommandVerdict =
  | { readonly allowed: true; readonly command: string }
  | {
      readonly allowed: false;
      readonly command: string;
      /** Actionable explanation suitable for returning to the agent. */
      readonly reason: string;
      /** True iff the refusal is from the immutable hard denylist. */
      readonly hardDenied: boolean;
    };

/** The override lists feeding {@link buildPolicy} (already split into entries). */
export interface PolicyOverrides {
  /** Command names to ADD to the allowlist. */
  readonly allow?: readonly string[];
  /** Command names to REMOVE from the allowlist. */
  readonly deny?: readonly string[];
}

/**
 * Build a {@link ResolvedPolicy} from the built-in defaults plus override lists.
 * Pure: it performs no I/O and does not read the environment. Every entry is
 * normalised on the way in, so the resolved sets compare cleanly against a
 * normalised command name.
 */
export function buildPolicy(overrides: PolicyOverrides = {}): ResolvedPolicy {
  const allow = new Set<string>(DEFAULT_ALLOWLIST);
  for (const entry of overrides.allow ?? []) {
    const name = normalizeCommandName(entry);
    if (name !== '') allow.add(name);
  }
  const deny = new Set<string>();
  for (const entry of overrides.deny ?? []) {
    const name = normalizeCommandName(entry);
    if (name !== '') deny.add(name);
  }
  return { allow, deny };
}

/**
 * Decide whether `command` may run under `policy`. PURE — the whole point of the
 * engine — so the verdict is a deterministic function of (policy, name). Layers,
 * in precedence:
 *
 *   1. Hard denylist  → refused, `hardDenied: true`. Checked FIRST, so no allow
 *      override can ever resurrect it.
 *   2. Override deny  → refused (fail-closed: deny wins over allow).
 *   3. Allowlist      → allowed.
 *   4. Otherwise      → refused (default-deny; the command is simply unknown).
 */
export function decideCommand(policy: ResolvedPolicy, command: string): CommandVerdict {
  // Fail-closed on a non-string command. Unreachable through the zod-validated
  // qmp_execute tool, but this is a pure, reused function — refuse with a denied
  // verdict rather than throwing a TypeError.
  if (typeof command !== 'string') {
    return {
      allowed: false,
      command: String(command),
      hardDenied: false,
      reason:
        'The QMP command name must be a string. A non-string command is refused by the Command ' +
        'Policy (fail-closed). Pass the command as a string, e.g. "query-status".',
    };
  }

  const name = normalizeCommandName(command);

  if (HARD_DENYLIST.has(name)) {
    return {
      allowed: false,
      command: name,
      hardDenied: true,
      reason:
        `QMP command "${name}" is permanently denied: it is on the immutable hard denylist ` +
        '(it can exfiltrate guest/host memory, read or write host files, open host resources, ' +
        'or run arbitrary HMP). It can NEVER be enabled via QMP_MCP_ALLOW or a policy file. ' +
        'Use a purpose-built, audited tool if you genuinely need this capability.',
    };
  }

  if (policy.deny.has(name)) {
    return {
      allowed: false,
      command: name,
      hardDenied: false,
      reason:
        `QMP command "${name}" is denied by the Command Policy (it is in QMP_MCP_DENY or the ` +
        'policy file deny list). Remove it from the deny configuration if it is safe to run.',
    };
  }

  if (policy.allow.has(name)) {
    return { allowed: true, command: name };
  }

  return {
    allowed: false,
    command: name,
    hardDenied: false,
    reason:
      `QMP command "${name}" is not in the Command Policy allowlist. The generic qmp_execute tool ` +
      'only runs allowlisted commands. Add it via QMP_MCP_ALLOW or the policy file allow list if ' +
      'it is safe — but commands on the hard denylist can never be allowed.',
  };
}

/** Split a comma-separated override env var into trimmed, non-empty entries. */
function splitList(raw: string | undefined): string[] {
  if (raw === undefined) return [];
  return raw
    .split(',')
    .map((entry) => entry.trim())
    .filter((entry) => entry !== '');
}

/**
 * Shape of a policy file. `allow`/`deny` are optional string lists; unknown
 * top-level keys are rejected (`.strict()`) so a typo like `allows:` fails loudly
 * instead of silently doing nothing.
 */
const policyFileSchema = z
  .object({
    allow: z.array(z.string()).default([]),
    deny: z.array(z.string()).default([]),
  })
  .strict();

/**
 * Read and parse the YAML policy file at `path`. Fails closed with a
 * {@link PolicyError} that names `QMP_MCP_POLICY_FILE` on any problem: the file
 * cannot be read, the YAML is malformed, or its shape is not
 * `{ allow?: string[], deny?: string[] }`. Returns the raw (un-normalised) lists.
 */
export function loadPolicyFile(path: string): { allow: string[]; deny: string[] } {
  let text: string;
  try {
    text = readFileSync(path, 'utf8');
  } catch (err) {
    throw new PolicyError(
      `QMP_MCP_POLICY_FILE could not be read: ${path} (${
        err instanceof Error ? err.message : String(err)
      }). Point it at a readable YAML file, or unset it to use the built-in default policy.`,
    );
  }

  let parsed: unknown;
  try {
    parsed = parseYaml(text);
  } catch (err) {
    throw new PolicyError(
      `QMP_MCP_POLICY_FILE is not valid YAML: ${path} (${
        err instanceof Error ? err.message : String(err)
      }). Fix the syntax, or unset it to use the built-in default policy.`,
    );
  }

  // An empty file parses to null/undefined — treat it as an empty policy.
  const data = parsed == null ? {} : parsed;
  const result = policyFileSchema.safeParse(data);
  if (!result.success) {
    throw new PolicyError(
      `QMP_MCP_POLICY_FILE has the wrong shape: ${path}. It must be a YAML mapping with optional ` +
        '"allow" and "deny" lists of command-name strings, e.g. `allow: [query-pci]`. ' +
        `(${result.error.issues.map((i) => `${i.path.join('.') || '<root>'}: ${i.message}`).join('; ')})`,
    );
  }
  return { allow: result.data.allow, deny: result.data.deny };
}

/**
 * Resolve the effective Command Policy from the environment: the built-in
 * defaults, overlaid with `QMP_MCP_ALLOW`/`QMP_MCP_DENY` and, when
 * `QMP_MCP_POLICY_FILE` is set, the YAML file's allow/deny lists. The only
 * impure entry point (reads env + filesystem). Throws {@link PolicyError} —
 * fail-closed — if the policy file is missing, unreadable, or malformed.
 *
 * Hard-denied commands named in an allow override are kept (they stay denied at
 * decision time) but logged as a warning, so an operator who tried to enable one
 * learns it was ignored rather than silently mis-trusting their config.
 */
export function resolveCommandPolicy(env: NodeJS.ProcessEnv): ResolvedPolicy {
  const allow = splitList(env.QMP_MCP_ALLOW);
  const deny = splitList(env.QMP_MCP_DENY);

  const filePath = env.QMP_MCP_POLICY_FILE?.trim();
  if (filePath) {
    const fromFile = loadPolicyFile(filePath);
    allow.push(...fromFile.allow);
    deny.push(...fromFile.deny);
  }

  for (const entry of allow) {
    if (HARD_DENYLIST.has(normalizeCommandName(entry))) {
      logger.warning(
        `Command Policy: "${entry.trim()}" is on the immutable hard denylist and cannot be ` +
          'allowed; ignoring its allow override. It remains denied.',
      );
    }
  }

  return buildPolicy({ allow, deny });
}
