// The bridge across, and which of the two shapes of it a document gets.
//
// The choice is made here, from the URL, and it has to be: a preload belongs to a
// **window**, chosen when the window is created, while the one window shows two
// documents — the launch screen before there is a gateway, and the client after.
// A preload picked at creation time can therefore only ever be right for one of
// them, and the half that got the wrong one was the launch screen: `remotexShell`
// was undefined, `launch.ts` threw on its first line, and the page sat on the
// markup it shipped with — "Starting the local gateway.", for a gateway that had
// already said why it would not start, with both its buttons dead.
//
// Still one bridge per document, which is the part that matters. The client is a
// window onto a machine somebody else is driving, and `remotexShell` reads and
// writes the file holding the credentials of every machine this app can reach; a
// document never sees both.
//
// Nothing else about Electron reaches either page: no `require`, no `process`, no
// IPC object. The renderer is sandboxed and this is the only way across.

import { contextBridge, ipcRenderer } from "electron";
import {
  CHANNEL,
  type ConfigSaveResult,
  type NativeCommand,
  type NativeEvent,
  SHELL_PATH_PREFIX,
  type ShellAction,
  type ShellStatus,
} from "../shared/contract.ts";

if (location.pathname.startsWith(SHELL_PATH_PREFIX)) {
  installShellBridge();
} else {
  installClientBridge();
}

/**
 * What the client sees: two globals, and nothing else.
 *
 * Both names are the client's to spell — see `frontend/src/gateway.ts` and
 * `frontend/src/nativeHost.ts`.
 */
function installClientBridge(): void {
  // Synchronous on purpose. `gateway.ts` reads this at module load to decide where
  // every request goes, so it has to be here before the document's first script
  // runs, and a preload is the last moment that is guaranteed to be.
  const gateway: string = ipcRenderer.sendSync(CHANNEL.gateway);

  contextBridge.exposeInMainWorld("__remotexGateway", gateway);

  contextBridge.exposeInMainWorld("remotexNative", {
    post: (event: NativeEvent) => {
      ipcRenderer.send(CHANNEL.event, event);
    },
    onCommand: (handler: (command: NativeCommand) => void) => {
      const listener = (_event: unknown, command: NativeCommand) =>
        handler(command);
      ipcRenderer.on(CHANNEL.command, listener);
      return () => {
        ipcRenderer.off(CHANNEL.command, listener);
      };
    },
  });
}

/** What the shell's own two documents see — the launch screen and the editor. */
function installShellBridge(): void {
  contextBridge.exposeInMainWorld("remotexShell", {
    // Returns its own disposer, like the client bridge's `onCommand`: a listener
    // removed by name rather than by channel, so detaching one page's handler
    // cannot take another's with it.
    onStatus: (handler: (status: ShellStatus) => void) => {
      const listener = (_event: unknown, status: ShellStatus) =>
        handler(status);
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
}
