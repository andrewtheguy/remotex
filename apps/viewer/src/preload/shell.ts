// The shell's own two documents — the launch screen and the configuration editor —
// get their own bridge, with nothing of the client's on it.
//
// Separate from `client.ts` rather than one preload with both: these pages can read
// and write the config file, and the client, which loads a page over a socket from a
// remote machine, must not be able to reach that even in principle.

import { contextBridge, ipcRenderer } from "electron";
import {
  CHANNEL,
  type ConfigSaveResult,
  type ShellAction,
  type ShellStatus,
} from "../shared/contract.ts";

contextBridge.exposeInMainWorld("remotexShell", {
  onStatus: (handler: (status: ShellStatus) => void) => {
    ipcRenderer.on(CHANNEL.shellStatus, (_event, status: ShellStatus) =>
      handler(status),
    );
  },
  act: (action: ShellAction) => {
    ipcRenderer.send(CHANNEL.shellAction, action);
  },
  readConfig: (): Promise<string> => ipcRenderer.invoke(CHANNEL.configRead),
  saveConfig: (text: string): Promise<ConfigSaveResult> =>
    ipcRenderer.invoke(CHANNEL.configSave, text),
});
