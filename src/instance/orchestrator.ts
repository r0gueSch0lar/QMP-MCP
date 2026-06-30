/**
 * The lifecycle states an Instance moves through. Only `NONE` is reachable in
 * this slice; the create/destroy transitions arrive with the lifecycle slice.
 */
export type InstanceState = 'NONE' | 'STARTING' | 'RUNNING' | 'PAUSED' | 'STOPPED';

/**
 * A read-only view of the current Instance for tools to return.
 */
export interface InstanceView {
  state: InstanceState;
}

/**
 * Holds the single managed Instance as a process-global singleton: exactly one
 * Instance exists at a time (see ADR-0001/0004). This slice only reports state;
 * later slices inject the QEMU driver port and add the create/destroy
 * transitions through this same seam.
 */
class Orchestrator {
  #state: InstanceState = 'NONE';

  /**
   * Return the current Instance view. Reports `NONE` when nothing is running.
   */
  getInstance(): InstanceView {
    return { state: this.#state };
  }
}

/** The process-global Orchestrator singleton. */
export const orchestrator = new Orchestrator();
