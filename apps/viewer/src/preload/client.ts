// What the client sees of the shell: two globals, and nothing else.
//
// Both names are the client's to spell — see `frontend/src/gateway.ts` and
// `frontend/src/nativeHost.ts`. Nothing about Electron reaches the page: no
// `require`, no `process`, no IPC object. The renderer is sandboxed and this is the
// only bridge across.

import { contextBridge, ipcRenderer } from "electron";
import {
  CHANNEL,
  type NativeCommand,
  type NativeEvent,
} from "../shared/contract.ts";

// Synchronous on purpose. `gateway.ts` reads this at module load to decide where
// every request goes, so it has to be here before the document's first script runs,
// and a preload is the last moment that is guaranteed to be.
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
