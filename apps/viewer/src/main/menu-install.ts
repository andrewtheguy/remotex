// The descriptor from `menu.ts`, turned into the menu macOS draws.
//
// All of Electron's involvement in the menu bar is here, and it is a translation and
// nothing more: no decisions, no derived titles, no enablement rules. Those are in
// `menu.ts`, where they can be tested without an app running.

import { Menu, type MenuItemConstructorOptions } from "electron";
import type { NativeCommand } from "../shared/contract.ts";
import {
  buildMenuSpec,
  type LocalAction,
  type MenuContext,
  type MenuItemSpec,
} from "./menu.ts";

export interface MenuActions {
  send(command: NativeCommand): void;
  local(action: LocalAction): void;
}

function toElectron(
  item: MenuItemSpec,
  actions: MenuActions,
): MenuItemConstructorOptions {
  if (item.separator) {
    return { type: "separator" };
  }
  const options: MenuItemConstructorOptions = { label: item.label };
  if (item.role) {
    options.role = item.role as MenuItemConstructorOptions["role"];
  }
  if (item.enabled !== undefined) {
    options.enabled = item.enabled;
  }
  if (item.checked !== undefined) {
    options.type = "checkbox";
    options.checked = item.checked;
  }
  if (item.submenu) {
    options.submenu = item.submenu.map((child) => toElectron(child, actions));
  }
  const command = item.command;
  const action = item.action;
  // A role's own behaviour is the point of having one, so a role is never overridden
  // by a click. Quit is the one item carrying both, and the role wins: macOS expects
  // to find `quit` in the application menu, and `action: "quit"` beside it is the
  // descriptor saying what the item does, never a second implementation of it.
  if (!item.role && (command || action)) {
    options.click = () => {
      if (command) {
        actions.send(command);
      } else if (action) {
        actions.local(action);
      }
    };
  }
  return options;
}

/**
 * Rebuild the whole menu bar.
 *
 * Rebuilt rather than mutated on every state post, which is the same reason the
 * descriptor is pure: an item's title, tick and enablement are all functions of the
 * reported state, and updating them one at a time is how a menu comes to disagree
 * with the thing on screen.
 */
export function installMenu(context: MenuContext, actions: MenuActions): void {
  const template = buildMenuSpec(context).map((menu) => ({
    label: menu.label,
    submenu: menu.items.map((item) => toElectron(item, actions)),
  }));
  Menu.setApplicationMenu(Menu.buildFromTemplate(template));
}
