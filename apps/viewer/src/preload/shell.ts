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
  // Returns its own disposer, like the client bridge's `onCommand`: a listener
  // removed by name rather than by channel, so detaching one page's handler cannot
  // take another's with it.
  onStatus: (handler: (status: ShellStatus) => void) => {
    const listener = (_event: unknown, status: ShellStatus) => handler(status);
    ipcRenderer.on(CHANNEL.shellStatus, listener);
    return () => {
      ipcRenderer.removeListener(CHANNEL.shellStatus, listener);
    };
  },
  act: (action: ShellAction) => {
    ipcRenderer.send(CHANNEL.shellAction, action);
  },
  readConfig: (): Promise<string> => ipcRenderer.invoke(CHANNEL.configRead),
  saveConfig: (text: string): Promise<ConfigSaveResult> =>
    ipcRenderer.invoke(CHANNEL.configSave, text),
});
