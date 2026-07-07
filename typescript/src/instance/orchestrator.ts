/**
 * The single-instance lifecycle Orchestrator (ADR-0001/0004). It holds the one
 * managed Instance and drives it through its lifecycle:
 *
 *   NONE → STARTING → RUNNING ⇄ PAUSED → STOPPED → NONE
 *
 * It implements create/destroy (NONE → STARTING → RUNNING → STOPPED → NONE), the
 * RUNNING ⇄ PAUSED pause/resume transitions, and the in-place control commands
 * (reset, ACPI powerdown, block/CPU queries, screendump) — each issued to the
 * current Instance's QMP Session through its {@link InstanceProcess}.
 *
 * The Orchestrator depends on the {@link QemuDriver} port by constructor
 * injection, so its whole lifecycle is testable against the fake driver. The
 * process-global {@link orchestrator} singleton wires in the real driver.
 */

import { randomInt, randomUUID } from 'node:crypto';
import { mkdir, readFile, rm, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import {
  type PortRange,
  resolveAllowHostNet,
  resolveAllowRawArgs,
  resolveAutoStart,
  resolveEventBufferSize,
  resolveHostfwdPortRange,
  resolveImageDir,
  resolveIsoDir,
  resolveMaxMemoryMb,
  resolveMaxVcpus,
  resolveQemuBinary,
  resolveViewerHost,
  resolveViewerPassword,
  resolveViewerPort,
} from '../config.js';
import { logger } from '../logger.js';
import {
  buildPolicy,
  CommandPolicyError,
  decideCommand,
  type ResolvedPolicy,
  resolveCommandPolicy,
} from '../policy/command-policy.js';
import type { InstanceProcess, QemuDriver } from '../qemu/driver.js';
import { RealQemuDriver } from '../qemu/real-driver.js';
import {
  startViewer as startRealViewer,
  type Viewer,
  type ViewerOptions,
} from '../viewer/viewer.js';
import {
  DEFAULT_EVENT_BUFFER_SIZE,
  EventBuffer,
  type ReadResult,
  type WaitForEventResult,
} from './event-buffer.js';
import {
  type Accel,
  buildArgv,
  type HardwareSpec,
  parseHardwareSpec,
  probeKvm,
  resolveAccel,
  VNC_LOOPBACK_HOST,
  VNC_LOOPBACK_PORT,
} from './hardware-spec.js';

/**
 * The lifecycle states an Instance moves through. `PAUSED` is entered by
 * {@link Orchestrator.pauseInstance} (QMP `stop`) — and by
 * {@link Orchestrator.createInstance} when the Guest loads frozen at the `-S`
 * startup pause (the default, unless `QMP_MCP_AUTO_START` resumes it) — and is left
 * by {@link Orchestrator.resumeInstance} (QMP `cont`). In `PAUSED` the Guest CPUs are
 * not executing, which `get_status`/`query-status` reports as `paused`/`prelaunch`.
 */
export type InstanceState = 'NONE' | 'STARTING' | 'RUNNING' | 'PAUSED' | 'STOPPED';

/** A read-only view of the current Instance for tools to return. */
export interface InstanceView {
  state: InstanceState;
  /** The validated Hardware Spec, when an Instance exists. */
  spec?: HardwareSpec;
  /** The accelerator the running Instance was launched with. */
  accel?: Accel;
}

/** The result of a successful {@link Orchestrator.createInstance}. `RUNNING` when
 * the Guest was auto-started (`QMP_MCP_AUTO_START`), else `PAUSED` (loaded, frozen
 * at `-S` until `resume_instance`). */
export interface CreateInstanceResult {
  state: 'RUNNING' | 'PAUSED';
  /** The validated Hardware Spec the Instance was built from. */
  spec: HardwareSpec;
  /** The accelerator actually chosen (KVM or TCG). */
  accel: Accel;
  /** Why that accelerator was chosen — reported to the agent (ADR-0008). */
  accelReason: string;
}

/**
 * A captured Instance screenshot. The image bytes are returned inline (base64)
 * rather than as a host path: the agent never learns or controls where the file
 * lived, and the server deletes it after reading (see
 * {@link Orchestrator.screendump}).
 */
export interface ScreendumpResult {
  /** MIME type of the captured image. */
  mimeType: string;
  /** Base64-encoded image bytes, ready to hand back as MCP image content. */
  data: string;
  /** Size of the decoded image in bytes. */
  bytes: number;
}

/** Knobs the Orchestrator needs that are not part of the Hardware Spec. */
export interface OrchestratorOptions {
  /** The `qemu-system-*` binary to launch. */
  binary: string;
  /** Server-managed path of the QMP UNIX socket. */
  qmpSocketPath: string;
  /**
   * Absolute path of the Image Store directory (ADR-0006). Disk names in the
   * spec are resolved against it when building the argv. Optional: a diskless
   * spec never needs it.
   */
  imageDir?: string;
  /**
   * Absolute path of the read-only ISO Store directory (ADR-0006). A cdrom's ISO
   * name in the spec is resolved against it when building the argv. Optional: a
   * spec with no cdrom never needs it.
   */
  isoDir?: string;
  /**
   * Inclusive host-port range a user-mode port-forward's `hostPort` must fall
   * within (ADR-0009). Optional: defaults to {@link DEFAULT_HOSTFWD_PORT_RANGE}
   * inside the argv builder when omitted.
   */
  hostfwdPortRange?: PortRange;
  /**
   * Whether host-level (`tap`/`bridge`) networking is permitted (ADR-0009).
   * Optional: defaults to false (host networking refused) when omitted.
   */
  allowHostNet?: boolean;
  /**
   * Whether `create_instance` auto-starts the Guest by issuing QMP `cont` right
   * after launch (`QMP_MCP_AUTO_START`, issue #8). Optional: defaults to false —
   * the Guest stays frozen at the `-S` startup pause until `resume_instance`.
   */
  autoStart?: boolean;
  /**
   * Hard cap, in MiB, on the spec's `memoryMb` (`QMP_MCP_MAX_MEMORY_MB`, issue
   * #9). An over-cap spec is rejected before qemu is spawned. Optional: omitted
   * means no memory cap is enforced (the singleton always injects it).
   */
  maxMemoryMb?: number;
  /**
   * Hard cap on the spec's `vcpus` (`QMP_MCP_MAX_VCPUS`, issue #9). An over-cap
   * spec is rejected before qemu is spawned. Optional: omitted means no vCPU cap
   * is enforced (the singleton always injects it).
   */
  maxVcpus?: number;
  /**
   * Whether the raw-args escape hatch is enabled (`QMP_MCP_ALLOW_RAW_ARGS`,
   * ADR-0002). When true a spec's `extraArgs` are appended to the generated argv;
   * when false (the default) a spec carrying `extraArgs` is rejected before qemu
   * is spawned. Optional: omitted means the hatch is closed (the singleton always
   * injects the env-resolved value).
   */
  allowRawArgs?: boolean;
  /**
   * The resolved Command Policy that governs which QMP commands the generic
   * {@link Orchestrator.executeCommand} path may run (ADR-0003). Optional: when
   * omitted, the built-in default-safe allowlist is used (the singleton injects
   * the env/file-resolved policy).
   */
  commandPolicy?: ResolvedPolicy;
  /**
   * Capacity of the Event Buffer that captures the Instance's QMP async events
   * (`QMP_MCP_EVENT_BUFFER_SIZE`, issue #12). Optional: defaults to
   * {@link DEFAULT_EVENT_BUFFER_SIZE} when omitted (the singleton injects the
   * env-resolved value).
   */
  eventBufferSize?: number;
  /**
   * The human-facing noVNC Viewer password (`QMP_MCP_VIEWER_PASSWORD`, ADR-0010).
   * A `display: vnc` spec is REJECTED before qemu is spawned when this is unset —
   * the fail-closed coupling between the Display and its browser Viewer. Optional:
   * omitted means the Viewer cannot be requested (the singleton injects it).
   */
  viewerPassword?: string;
  /**
   * Address the Viewer's own HTTP server binds to (`QMP_MCP_VIEWER_HOST`, default
   * `127.0.0.1`). Independent of the MCP transport, so the Viewer works under
   * `TRANSPORT=stdio` (ADR-0010).
   */
  viewerHost?: string;
  /** TCP port the Viewer's HTTP server listens on (`QMP_MCP_VIEWER_PORT`, default 6080). */
  viewerPort?: number;
  /**
   * Factory that starts the noVNC Viewer for a `display: vnc` Instance. Injected so
   * the lifecycle is testable without binding a real port; the singleton wires in
   * the real in-process Viewer.
   */
  startViewer?: (options: ViewerOptions) => Promise<Viewer>;
  /** Probe for KVM availability (injected for testability). */
  kvmAvailable: () => boolean;
  /**
   * Predicate for "is the QMP socket path already occupied". Injected so the
   * refuse-if-occupied branch is testable without touching the filesystem.
   */
  socketOccupied: (path: string) => Promise<boolean>;
}

/**
 * Raised for lifecycle violations (e.g. creating while an Instance exists). The
 * message is always actionable: it tells the agent what to do next.
 */
export class LifecycleError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'LifecycleError';
  }
}

/**
 * Default `wait_for_event` timeout when a caller supplies none (issue #12). A
 * long-poll horizon: long enough to catch a boot/shutdown, short enough that the
 * agent regains control to poll again.
 */
const DEFAULT_WAIT_TIMEOUT_MS = 30_000;

/** Default QMP socket path: a per-server file under the OS runtime/temp dir. */
export function defaultQmpSocketPath(): string {
  return join(tmpdir(), 'qmp-mcp', 'qmp.sock');
}

/** Default occupied-check: the path exists (as a socket or anything else). */
export async function defaultSocketOccupied(path: string): Promise<boolean> {
  try {
    await stat(path);
    return true;
  } catch {
    return false;
  }
}

/**
 * Holds the single managed Instance as a process-global singleton: exactly one
 * Instance exists at a time. Requesting a new Instance while one exists is
 * rejected rather than auto-replaced (ADR-0004).
 */
export class Orchestrator {
  #driver: QemuDriver;
  #options: OrchestratorOptions;
  /** The Command Policy gating {@link executeCommand}; defaults to the allowlist. */
  #commandPolicy: ResolvedPolicy;
  #state: InstanceState = 'NONE';
  #process?: InstanceProcess;
  #spec?: HardwareSpec;
  #accel?: Accel;
  /** The running noVNC Viewer for a `display: vnc` Instance, if any (ADR-0010). */
  #viewer?: Viewer;
  /** Factory that starts the Viewer; the real in-process Viewer by default. */
  #startViewer: (options: ViewerOptions) => Promise<Viewer>;
  /**
   * The Event Buffer capturing the current Instance's QMP async events. One
   * buffer lives for the server's lifetime; it is {@link EventBuffer.reset} on
   * every create/destroy so events never bleed across Instances (issue #12).
   */
  #eventBuffer: EventBuffer;
  /** Unsubscribes the buffer from the current Instance's event stream. */
  #unsubscribeEvents?: () => void;
  /**
   * Identifies the create_instance call that currently owns the reserved slot.
   * A call only mutates the singleton's fields while its own token is installed,
   * so a superseded launch cannot clobber a slot another call has since taken.
   */
  #launchToken?: symbol;

  constructor(driver: QemuDriver, options: OrchestratorOptions) {
    this.#driver = driver;
    this.#options = options;
    // Resolve the policy once: an omitted policy means the built-in allowlist.
    this.#commandPolicy = options.commandPolicy ?? buildPolicy();
    this.#eventBuffer = new EventBuffer(options.eventBufferSize ?? DEFAULT_EVENT_BUFFER_SIZE);
    // Default to the real in-process Viewer; tests inject a fake factory.
    this.#startViewer = options.startViewer ?? startRealViewer;
  }

  /** Return the current Instance view. Reports `NONE` when nothing is running. */
  getInstance(): InstanceView {
    return { state: this.#state, spec: this.#spec, accel: this.#accel };
  }

  /**
   * Build and launch a new Instance from an untrusted candidate Hardware Spec,
   * negotiate its QMP Session, and bring it to `RUNNING`. Rejects when an
   * Instance already exists or the QMP socket path is occupied.
   */
  async createInstance(candidate: unknown): Promise<CreateInstanceResult> {
    if (this.#state !== 'NONE') {
      throw new LifecycleError(
        `An Instance already exists (state ${this.#state}). Only one Instance may run at a time — ` +
          'destroy it with destroy_instance before creating a new one.',
      );
    }

    // Reserve the single slot SYNCHRONOUSLY — before any await (spec parse, accel
    // probe, socket check, launch) — so two concurrent create_instance calls
    // cannot both pass the NONE guard above and spawn two qemu, orphaning one.
    this.#state = 'STARTING';
    const token = Symbol('launch');
    this.#launchToken = token;
    const ownsSlot = (): boolean => this.#launchToken === token;
    // Release the slot back to NONE, but only while this call still owns it.
    const release = (): void => {
      if (!ownsSlot()) return;
      this.#launchToken = undefined;
      this.#process = undefined;
      this.#spec = undefined;
      this.#accel = undefined;
      this.#state = 'NONE';
    };

    try {
      // Parse/accel are synchronous but may throw; a throw must free the slot.
      const spec = parseHardwareSpec(candidate);
      // Fail-closed coupling (ADR-0010): a vnc Display starts the browser Viewer,
      // which cannot serve without QMP_MCP_VIEWER_PASSWORD. Refuse BEFORE spawning
      // qemu, so nothing is launched when the Viewer could never front it.
      if (spec.display === 'vnc') this.#assertViewerConfigured();
      const resolution = resolveAccel(spec.accel, this.#options.kvmAvailable);

      const { qmpSocketPath, binary } = this.#options;
      if (await this.#options.socketOccupied(qmpSocketPath)) {
        throw new LifecycleError(
          `The QMP socket path ${qmpSocketPath} is already occupied — refusing to start rather than ` +
            'clobber or adopt a process this server did not launch. Remove the stale socket (or stop the ' +
            'other process), then retry.',
        );
      }

      const argv = buildArgv(spec, {
        accel: resolution.accel,
        qmpSocketPath,
        imageDir: this.#options.imageDir,
        isoDir: this.#options.isoDir,
        hostfwdPortRange: this.#options.hostfwdPortRange,
        allowHostNet: this.#options.allowHostNet,
        maxMemoryMb: this.#options.maxMemoryMb,
        maxVcpus: this.#options.maxVcpus,
        allowRawArgs: this.#options.allowRawArgs,
      });
      logger.info(`creating Instance (machine=${spec.machine}, accel=${resolution.accel})`);

      let process: InstanceProcess;
      try {
        process = await this.#driver.launch({ binary, argv, qmpSocketPath });
      } catch (err) {
        throw new LifecycleError(
          `Failed to create the Instance: ${err instanceof Error ? err.message : String(err)}`,
        );
      }

      // If we lost ownership while launch was in flight (e.g. a concurrent
      // destroy/reset), do NOT clobber the new owner's state; tear down the
      // process we just launched so it is not orphaned.
      if (!ownsSlot()) {
        void process.close().catch(() => undefined);
        throw new LifecycleError(
          'Instance creation was superseded before it completed; the launched process was torn down.',
        );
      }

      // For a vnc Display, arm the Display password over QMP and start the Viewer
      // BEFORE publishing the Instance as RUNNING, so any failure tears everything
      // down and leaves state NONE (fail-closed).
      let viewer: Viewer | undefined;
      if (spec.display === 'vnc') {
        viewer = await this.#armDisplay(process);
        // A concurrent teardown could have superseded us during those awaits; if so,
        // do not clobber the new owner — tear down what we just started.
        if (!ownsSlot()) {
          await viewer.stop().catch(() => undefined);
          void process.close().catch(() => undefined);
          throw new LifecycleError(
            'Instance creation was superseded before it completed; the launched process was torn down.',
          );
        }
      }

      this.#process = process;
      this.#viewer = viewer;
      this.#spec = spec;
      this.#accel = resolution.accel;
      // Auto-start (issues #8, #10) decides the lifecycle state create lands in.
      // The Guest launches frozen at the `-S` startup pause; unless we resume it
      // below it is NOT executing, so the honest lifecycle state is PAUSED — which
      // agrees with get_status/query-status (the Guest reads paused/prelaunch until
      // resume_instance). With QMP_MCP_AUTO_START on we `cont` it and land in RUNNING.
      const started = this.#options.autoStart === true;
      this.#state = started ? 'RUNNING' : 'PAUSED';
      this.#launchToken = undefined;
      // Start a fresh Event Buffer for this Instance and capture its QMP async
      // events from here on — no events carry over from a previous Instance.
      this.#eventBuffer.reset();
      this.#unsubscribeEvents = process.onEvent((event) => this.#eventBuffer.append(event));
      // If the process exits on its own, reflect that the Instance is gone.
      void process.exited.then(() => this.#onProcessExit());

      // When auto-start is on, resume the Guest now (QMP `cont`) so it begins
      // executing; done AFTER event capture is wired so the boot's QMP events are
      // recorded. Otherwise it stays PAUSED, frozen at `-S`, until resume_instance.
      if (started) {
        await process.execute('cont');
        logger.info(`Instance RUNNING (auto-started; ${resolution.reason})`);
      } else {
        logger.info(
          `Instance PAUSED — loaded, frozen at the -S startup pause; call resume_instance ` +
            `to start it (${resolution.reason})`,
        );
      }
      return {
        state: started ? 'RUNNING' : 'PAUSED',
        spec,
        accel: resolution.accel,
        accelReason: resolution.reason,
      };
    } catch (err) {
      release();
      throw err;
    }
  }

  /**
   * Terminate the running Instance's `qemu-system-*` process, close its QMP
   * Session, and return to `NONE`. Rejects when no Instance exists.
   */
  async destroyInstance(): Promise<{ state: 'NONE' }> {
    if (this.#state === 'NONE' || !this.#process) {
      throw new LifecycleError(
        'No Instance is running, so there is nothing to destroy. Create one with create_instance first.',
      );
    }
    // Claim the teardown SYNCHRONOUSLY: capture the handle and clear it (and the
    // slot/spec/accel) before the first await, so a concurrent destroyInstance
    // hits the no-Instance guard above instead of double-closing.
    const process = this.#process;
    const viewer = this.#viewer;
    this.#process = undefined;
    this.#viewer = undefined;
    this.#spec = undefined;
    this.#accel = undefined;
    this.#launchToken = undefined;
    this.#state = 'STOPPED';
    // Detach from the Instance's event stream and clear the buffer (settling any
    // pending wait_for_event as a clean timeout); events do not outlive the Instance.
    this.#unsubscribeEvents?.();
    this.#unsubscribeEvents = undefined;
    this.#eventBuffer.reset();
    logger.info('destroying Instance');
    try {
      // Stop the Viewer alongside qemu — its lifetime equals the Instance's (ADR-0010).
      await viewer?.stop().catch(() => undefined);
      await process.close();
    } finally {
      this.#state = 'NONE';
    }
    logger.info('Instance destroyed (state NONE)');
    return { state: 'NONE' };
  }

  /**
   * Return the live QMP `query-status` result for the running Instance. Rejects
   * when no Instance is running.
   */
  async getStatus(): Promise<unknown> {
    return this.#requireInstance('query its status').execute('query-status');
  }

  /**
   * Return the Instance's recently buffered QMP async events WITHOUT blocking
   * (the `get_events` tool). Cursor-based: with no `since`, returns every buffered
   * event plus a `cursor`; passing that `cursor` back as `since` next time pages
   * forward without missing or repeating events. Rejects when no Instance runs.
   */
  getEvents(since?: number): ReadResult {
    this.#requireInstance('read its events');
    return this.#eventBuffer.read(since);
  }

  /**
   * Long-poll for a matching QMP async event (the `wait_for_event` tool). Resolves
   * — never rejects — with the first matching event, or with `{ timedOut: true }`
   * once `timeoutMs` elapses (a timeout is a NORMAL outcome). With no `eventName`
   * any event matches. Pass `sinceCursor` (a prior `cursor`) to also consider
   * already-buffered events, so an event that arrived between calls is not lost;
   * without it the wait is future-only. Rejects only when no Instance runs.
   */
  async waitForEvent(opts: {
    eventName?: string;
    timeoutMs?: number;
    sinceCursor?: number;
  }): Promise<WaitForEventResult> {
    this.#requireInstance('wait for its events');
    return this.#eventBuffer.waitFor({
      eventName: opts.eventName,
      sinceCursor: opts.sinceCursor,
      timeoutMs: opts.timeoutMs ?? DEFAULT_WAIT_TIMEOUT_MS,
    });
  }

  /**
   * Pause the running Instance's Guest CPUs via QMP `stop`, moving the lifecycle
   * RUNNING → PAUSED (reflected by `get_status`/`query-status`, which then reports
   * `paused`). Idempotent: pausing an already-PAUSED Instance re-issues the
   * harmless `stop` and stays PAUSED. Rejects when no Instance is running.
   */
  async pauseInstance(): Promise<{ state: 'PAUSED' }> {
    const process = this.#requireInstance('pause it');
    await process.execute('stop');
    this.#state = 'PAUSED';
    logger.info('Instance PAUSED (QMP stop)');
    return { state: 'PAUSED' };
  }

  /**
   * Resume the Instance's Guest CPUs via QMP `cont`, moving the lifecycle
   * PAUSED → RUNNING. Idempotent: resuming an already-RUNNING Instance re-issues
   * the harmless `cont` and stays RUNNING. Rejects when no Instance is running.
   */
  async resumeInstance(): Promise<{ state: 'RUNNING' }> {
    const process = this.#requireInstance('resume it');
    await process.execute('cont');
    this.#state = 'RUNNING';
    logger.info('Instance RUNNING (QMP cont)');
    return { state: 'RUNNING' };
  }

  /**
   * Hard-reset the Instance via QMP `system_reset` (equivalent to the reset
   * button). This reboots the Guest in place; it does not change the lifecycle
   * state. Rejects when no Instance is running.
   */
  async resetInstance(): Promise<{ state: InstanceState }> {
    const process = this.#requireInstance('reset it');
    await process.execute('system_reset');
    logger.info('Instance reset (QMP system_reset)');
    return { state: this.#state };
  }

  /**
   * Request a graceful Guest shutdown via QMP `system_powerdown` (sends an ACPI
   * power-button event). This only *asks* the Guest to power off; the Instance
   * keeps running until the Guest acts, so the lifecycle state is unchanged.
   * Rejects when no Instance is running.
   */
  async powerdownInstance(): Promise<{ state: InstanceState }> {
    const process = this.#requireInstance('power it down');
    await process.execute('system_powerdown');
    logger.info('Instance ACPI powerdown requested (QMP system_powerdown)');
    return { state: this.#state };
  }

  /** Return the live QMP `query-block` result. Rejects when no Instance runs. */
  async queryBlock(): Promise<unknown> {
    return this.#requireInstance('list its block devices').execute('query-block');
  }

  /** Return the live QMP `query-cpus-fast` result. Rejects when no Instance runs. */
  async queryCpus(): Promise<unknown> {
    return this.#requireInstance('query its CPUs').execute('query-cpus-fast');
  }

  /**
   * Capture a screenshot of the Instance's display via QMP `screendump` and
   * return the image inline.
   *
   * SECURITY: QMP `screendump` writes an arbitrary host file at the path it is
   * given, so the `filename` is ALWAYS server-chosen — a fresh, unique file under
   * a server-controlled directory — and never agent-supplied (the method takes no
   * path input). The bytes are read back, returned as base64, and the temp file is
   * deleted, so the agent never learns or controls a host path. Rejects when no
   * Instance is running.
   */
  async screendump(): Promise<ScreendumpResult> {
    const process = this.#requireInstance('capture a screendump');
    const dir = join(tmpdir(), 'qmp-mcp', 'screendumps');
    await mkdir(dir, { recursive: true });
    // Server-chosen, unguessable, single-use path — NOT influenced by the agent.
    const filename = join(dir, `screendump-${randomUUID()}.png`);
    try {
      await process.execute('screendump', { filename, format: 'png' });
      const bytes = await readFile(filename);
      return { mimeType: 'image/png', data: bytes.toString('base64'), bytes: bytes.length };
    } finally {
      // Best-effort cleanup: never leave the captured frame on the host.
      await rm(filename, { force: true }).catch(() => undefined);
    }
  }

  /**
   * Run a generic QMP command against the running Instance, gated by the Command
   * Policy (ADR-0003). The command name is checked FIRST: a denied command throws
   * a {@link CommandPolicyError} and never reaches the QMP Session — fail-closed,
   * and so a hard-denied command is refused even with no Instance running. Only an
   * allowed command requires (and is forwarded to) the live Session, returning its
   * QMP `return` value. The forwarded name is the normalised one, so trailing
   * whitespace never reaches QEMU.
   */
  async executeCommand(command: string, args?: Record<string, unknown>): Promise<unknown> {
    const verdict = decideCommand(this.#commandPolicy, command);
    if (!verdict.allowed) {
      throw new CommandPolicyError(verdict.reason, verdict.hardDenied);
    }
    const process = this.#requireInstance(`execute the QMP command "${verdict.command}"`);
    return process.execute(verdict.command, args);
  }

  /**
   * Return the live {@link InstanceProcess} for an action that requires a running
   * Instance, or throw an actionable {@link LifecycleError} naming the action when
   * none exists. The handle is only present in RUNNING/PAUSED, so this also
   * fail-closes the STARTING/STOPPED/NONE cases.
   */
  #requireInstance(action: string): InstanceProcess {
    if (!this.#process) {
      throw new LifecycleError(
        `No Instance is running, so there is nothing to ${action}. ` +
          'Create one with create_instance first.',
      );
    }
    return this.#process;
  }

  /**
   * Enforce the fail-closed Display↔Viewer coupling (ADR-0010): a `display: vnc`
   * spec requires `QMP_MCP_VIEWER_PASSWORD`. Throws an actionable
   * {@link LifecycleError} naming the variable when it is unset, so create_instance
   * is refused before qemu is spawned.
   */
  #assertViewerConfigured(): void {
    const password = this.#options.viewerPassword;
    if (password === undefined || password === '') {
      throw new LifecycleError(
        'The Hardware Spec requested display "vnc", which starts the noVNC Viewer, but ' +
          'QMP_MCP_VIEWER_PASSWORD is not set. Set QMP_MCP_VIEWER_PASSWORD to a strong password to ' +
          'enable the Viewer, or use display "none" for a headless Instance.',
      );
    }
  }

  /**
   * Arm the vnc Display and start its Viewer. A fresh VNC password is generated and
   * set over QMP (`set_password`) — never placed in argv, so it stays out of `ps` —
   * then handed to the Viewer to embed in the post-auth page for auto-authentication.
   * On any failure the launched qemu (and a half-started Viewer) are torn down and a
   * {@link LifecycleError} is thrown, so the Instance never reaches RUNNING half-armed.
   */
  async #armDisplay(process: InstanceProcess): Promise<Viewer> {
    const vncPassword = generateVncPassword();
    let viewer: Viewer | undefined;
    try {
      await process.execute('set_password', { protocol: 'vnc', password: vncPassword });
      viewer = await this.#startViewer({
        host: this.#options.viewerHost ?? '127.0.0.1',
        port: this.#options.viewerPort ?? 6080,
        // #assertViewerConfigured() guaranteed a non-empty password before launch.
        password: this.#options.viewerPassword ?? '',
        vncHost: VNC_LOOPBACK_HOST,
        vncPort: VNC_LOOPBACK_PORT,
        vncPassword,
      });
      return viewer;
    } catch (err) {
      await viewer?.stop().catch(() => undefined);
      void process.close().catch(() => undefined);
      throw new LifecycleError(
        `Failed to start the noVNC Viewer for the vnc Display: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
  }

  /** Reconcile state when the process exits without an explicit destroy. */
  #onProcessExit(): void {
    if (this.#state === 'STOPPED' || this.#state === 'NONE') return;
    logger.warning('Instance process exited unexpectedly; resetting state to NONE');
    const process = this.#process;
    const viewer = this.#viewer;
    this.#process = undefined;
    this.#viewer = undefined;
    this.#spec = undefined;
    this.#accel = undefined;
    this.#launchToken = undefined;
    this.#state = 'NONE';
    // The Instance is gone: stop capturing and clear the buffer (settling any
    // pending wait_for_event), so no events bleed into the next Instance.
    this.#unsubscribeEvents?.();
    this.#unsubscribeEvents = undefined;
    this.#eventBuffer.reset();
    // Release the handle so the managed QMP socket is removed. A crashed/SIGKILLed
    // qemu leaves its socket file behind; without this, every future create would
    // refuse with 'occupied'. close() is idempotent and best-effort here.
    void process?.close().catch(() => undefined);
    // The Instance is gone: stop its Viewer too (ADR-0010).
    void viewer?.stop().catch(() => undefined);
  }
}

/**
 * Generate the internal VNC Display password. The VNC auth scheme truncates the
 * password to 8 characters, so 8 alphanumerics is the effective maximum; QEMU and
 * noVNC truncate identically, so the armed and embedded passwords always match.
 * Ambiguous glyphs (0/O, 1/l/I) are omitted so it stays copy-safe if ever surfaced.
 */
function generateVncPassword(): string {
  const alphabet = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789';
  // crypto.randomInt(max) draws a uniform integer in [0, max) with rejection
  // sampling, so it has none of the modulo bias `randomBytes[i] % length` would
  // introduce when 256 is not a multiple of the alphabet length (it is not: 55).
  return Array.from({ length: 8 }, () => alphabet.charAt(randomInt(alphabet.length))).join('');
}

/** The process-global Orchestrator singleton, wired to the real QEMU driver. */
export const orchestrator = new Orchestrator(new RealQemuDriver(), {
  // argv[0] for the launched guest; QMP_MCP_QEMU_BINARY selects the guest
  // architecture (e.g. qemu-system-aarch64), while the spec's machine/cpu still
  // shape the rest of the argv (issue #15).
  binary: resolveQemuBinary(process.env),
  qmpSocketPath: defaultQmpSocketPath(),
  // Resolve disk names against the configured Image Store (ADR-0006).
  imageDir: resolveImageDir(process.env),
  // Resolve cdrom ISO names against the configured read-only ISO Store (ADR-0006).
  isoDir: resolveIsoDir(process.env),
  // Bound user-mode port-forwards and gate host networking (ADR-0009).
  hostfwdPortRange: resolveHostfwdPortRange(process.env),
  allowHostNet: resolveAllowHostNet(process.env),
  autoStart: resolveAutoStart(process.env),
  // Enforce the env-configurable memory/vCPU caps before launch (issue #9).
  maxMemoryMb: resolveMaxMemoryMb(process.env),
  maxVcpus: resolveMaxVcpus(process.env),
  // Gate the raw-args escape hatch: a spec's extraArgs are refused unless
  // QMP_MCP_ALLOW_RAW_ARGS=true (ADR-0002).
  allowRawArgs: resolveAllowRawArgs(process.env),
  // noVNC Viewer for a vnc Display (ADR-0010): the human-facing gate plus the
  // Viewer's own bind address/port. A vnc spec is refused when the password is unset.
  viewerPassword: resolveViewerPassword(process.env),
  viewerHost: resolveViewerHost(process.env),
  viewerPort: resolveViewerPort(process.env),
  // Bound the Event Buffer of recent QMP async events (issue #12).
  eventBufferSize: resolveEventBufferSize(process.env),
  // Resolve the Command Policy for the generic qmp_execute tool: the default-safe
  // allowlist plus QMP_MCP_ALLOW/DENY and the optional QMP_MCP_POLICY_FILE
  // overrides, with the immutable hard denylist always in force (ADR-0003, #11).
  commandPolicy: resolveCommandPolicy(process.env),
  // `/dev/kvm` probe (single source of truth) from the hardware-spec module.
  kvmAvailable: probeKvm,
  socketOccupied: defaultSocketOccupied,
});
