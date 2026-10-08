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

export type BackendEvent =
  | { type: 'hello'; version: number; subcommand: string }
  | { type: 'step'; step: string; state: 'start' | 'done' }
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
