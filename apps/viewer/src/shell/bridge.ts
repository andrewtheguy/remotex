// What `src/preload/shell.ts` exposes, declared once.
//
// Once, because both of the shell's documents see the same global: two `declare
// global` blocks describing halves of it are two descriptions to disagree, and
// TypeScript says so.

import type {
  ConfigSaveResult,
  ShellAction,
  ShellStatus,
} from "../shared/contract.ts";

export interface ShellBridge {
  onStatus(handler: (status: ShellStatus) => void): void;
  act(action: ShellAction): void;
  readConfig(): Promise<string>;
  saveConfig(text: string): Promise<ConfigSaveResult>;
}

declare global {
  interface Window {
    remotexShell: ShellBridge;
  }
}

export const shell: ShellBridge = window.remotexShell;
