import { useEffect, useState } from "react";

// The viewer and the served frontend ship in lockstep. Increment this whenever
// either side changes the shape or semantics of a native-host message.
export const NATIVE_HOST_BRIDGE_VERSION = 2;

interface NativeHostDescriptor {
  bridgeVersion: number;
  viewerVersion: string;
}

interface NativeMessageHandler {
  postMessage: (message: unknown) => Promise<unknown>;
}

interface NativeCommandResult {
  ok: boolean;
  error?: string;
}

export type NativeCommand =
  | { type: "key"; code: string; pressed: boolean; caps: boolean }
  | { type: "releaseKeys" }
  | { type: "clipboard"; text: string }
  | { type: "clipboardRequest" }
  | { type: "resize" }
  | { type: "switchTarget" }
  | { type: "logout" }
  | { type: "takeOver" };

export interface NativeHostState {
  screen: "checking" | "login" | "picker" | "desktop";
  connectionStatus:
    | "connecting"
    | "connected"
    | "reconnecting"
    | "busy"
    | "takenOver"
    | null;
  connectedTarget: string | null;
  guestOS: "windows" | "macos" | "linux" | null;
  canResize: boolean;
  canClipboard: boolean;
  canCaptureKeyboard: boolean;
}

export type NativeHostEvent =
  | {
      type: "ready";
      bridgeVersion: number;
      appVersion: string;
    }
  | { type: "state"; state: NativeHostState }
  | { type: "remoteClipboard"; text: string; seq: number };

declare global {
  interface Window {
    // Installed by the viewer at document start, before the SPA executes.
    __remotexNativeHost?: NativeHostDescriptor;
    // Installed by this module for structured native -> web commands.
    __remotexNativeDispatch?: (command: unknown) => NativeCommandResult;
    webkit?: {
      messageHandlers?: {
        remotexNative?: NativeMessageHandler;
      };
    };
  }
}

type CommandHandler = (command: NativeCommand) => NativeCommandResult;

let connected = false;
let commandHandler: CommandHandler | null = null;

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null
    ? (value as Record<string, unknown>)
    : null;
}

function parseCommand(value: unknown): NativeCommand | null {
  const command = record(value);
  if (!command || typeof command.type !== "string") {
    return null;
  }
  switch (command.type) {
    case "key":
      return typeof command.code === "string" &&
        typeof command.pressed === "boolean" &&
        typeof command.caps === "boolean"
        ? {
            type: "key",
            code: command.code,
            pressed: command.pressed,
            caps: command.caps,
          }
        : null;
    case "clipboard":
      return typeof command.text === "string"
        ? { type: "clipboard", text: command.text }
        : null;
    case "releaseKeys":
    case "clipboardRequest":
    case "resize":
    case "switchTarget":
    case "logout":
    case "takeOver":
      return { type: command.type };
    default:
      return null;
  }
}

window.__remotexNativeDispatch = (value: unknown): NativeCommandResult => {
  if (!connected) {
    return { ok: false, error: "native host handshake is incomplete" };
  }
  const command = parseCommand(value);
  if (!command) {
    return { ok: false, error: "invalid native command" };
  }
  if (!commandHandler) {
    return { ok: false, error: "command is unavailable on this screen" };
  }
  return commandHandler(command);
};

function handler(): NativeMessageHandler | null {
  return window.webkit?.messageHandlers?.remotexNative ?? null;
}

function accepted(value: unknown): boolean {
  const reply = record(value);
  return reply?.accepted === true;
}

async function sendRaw(event: NativeHostEvent): Promise<unknown> {
  const messageHandler = handler();
  if (!messageHandler) {
    throw new Error("native message handler is unavailable");
  }
  return messageHandler.postMessage(event);
}

// The one host-detection path. A WKWebView alone is not enough: both the
// injected descriptor and the reply-capable message handler must agree exactly
// with this frontend before native behavior is enabled.
export function useNativeHost(): boolean {
  const [available, setAvailable] = useState(false);

  useEffect(() => {
    const descriptor = window.__remotexNativeHost;
    if (
      descriptor?.bridgeVersion !== NATIVE_HOST_BRIDGE_VERSION ||
      descriptor.viewerVersion !== __APP_VERSION__ ||
      !handler()
    ) {
      connected = false;
      return;
    }

    let cancelled = false;
    void sendRaw({
      type: "ready",
      bridgeVersion: NATIVE_HOST_BRIDGE_VERSION,
      appVersion: __APP_VERSION__,
    })
      .then((reply) => {
        if (!cancelled && accepted(reply)) {
          connected = true;
          setAvailable(true);
        }
      })
      .catch(() => {
        // The ordinary web UI remains complete, including its floating menu.
      });

    return () => {
      cancelled = true;
      connected = false;
    };
  }, []);

  return available;
}

export function postNativeHostEvent(event: NativeHostEvent): void {
  if (!connected) {
    return;
  }
  void sendRaw(event).catch(() => {
    // A navigation or closing window can remove the handler between the
    // connected check and the post. The page remains usable without native UI.
  });
}

export function setNativeCommandHandler(handler: CommandHandler): () => void {
  commandHandler = handler;
  return () => {
    if (commandHandler === handler) {
      commandHandler = null;
    }
  };
}

export function isNativeHostConnected(): boolean {
  return connected;
}
