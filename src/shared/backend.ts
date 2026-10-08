export interface RiceListRow {
  name: string;
  display_name: string;
  creator_name: string;
  repo: string;
  install_supported: boolean;
  installed: boolean;
}

export type BackendCommand = 'preview' | 'install' | 'uninstall';

export interface BackendRunRequest {
  command: BackendCommand;
  name?: string;
}

/**
 * The configuration an `install` produced.
 *
 * On a declarative platform install cannot mutate the system, so the config *is*
 * the result — the UI has to show it rather than implying the shell was installed.
 */
export interface InstallConfig {
  /** Language of `text`, and so how to render it. */
  format: 'nix';
  text: string;
  /** Where the backend also wrote it, if it did. */
  path?: string;
}

export type BackendEvent =
  | { type: 'hello'; version: number; subcommand: string }
  | { type: 'step'; step: string; state: 'start' | 'done' }
  | ({ type: 'config' } & InstallConfig)
  | { type: 'success'; active?: string }
  | { type: 'fail'; stage: string; reason: string; log_tail?: string; plugins?: string[] };

export interface BackendRunResult {
  ok: boolean;
  events: BackendEvent[];
  rawTail: string[];
  exitCode: number | null;
}

export interface EnvironmentCheckResult {
  supported: boolean;
  conflictingShells: string[];
  /** Which package manager backs `install`: Arch packages, or Nix flakes. */
  platform: 'arch' | 'nix' | null;
  /** Detected compositor, or null when detection failed. */
  compositor: 'hyprland' | 'niri' | null;
  sessionType: string | null;
  /** Why `supported` is false, most actionable first. */
  reasons: string[];
}
