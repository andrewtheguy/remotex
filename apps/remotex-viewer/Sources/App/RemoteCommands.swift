import AppKit
import SwiftUI

struct RemoteCommands: Commands {
    let model: AppModel

    var body: some Commands {
        // `commandsReplaced` takes the standard app menu down with the rest, Quit
        // included, and an app you can only leave by closing its window is not one.
        // Put back deliberately, and — like every item below — without a chord: the
        // Command-Q the muscle memory reaches for belongs to the guest while a
        // desktop is focused, so advertising it on this item would be advertising a
        // shortcut that does something else. Moving focus off the desktop is what
        // makes the menu reachable from the keyboard again.
        // And About, for the same reason and with the same consequence if it is left
        // out: `commandsReplaced` removes the standard application menu whole, so the
        // one place a Mac app states its version had no item in it at all. Named after
        // the instance's `branding`, which is what the rest of its windows say.
        CommandGroup(replacing: .appInfo) {
            Button("About \(model.branding)") {
                model.showAbout()
            }
        }

        CommandGroup(replacing: .appTermination) {
            Button("Quit remotex") {
                NSApp.terminate(nil)
            }
        }

        // AppKit owns View/full-screen and the resize items inserted beside it.
        // Remote commands have no key equivalents because focused desktop capture
        // sends Command chords to the guest.
        CommandMenu("Remote") {
            Toggle(
                model.macOSKeyboardOverridesLabel,
                isOn: Binding(
                    get: { model.macOSKeyboardOverridesActive },
                    set: { model.macOSKeyboardOverridesEnabled = $0 }
                )
            )
            .disabled(model.session.remoteIsMac)

            Divider()

            Button(
                model.clipboard.isFetching
                    ? "Fetching Clipboard…"
                    : "Clipboard…"
            ) {
                model.clipboard.togglePanel()
            }
            .disabled(!model.clipboard.isEnabled || model.clipboard.isFetching)

            // Sound from the remote. Greyed rather than hidden for a target that has
            // none: a menu whose items come and go is harder to learn than one item
            // that is sometimes disabled.
            //
            // The item says nothing about whether sound is arriving, because from this
            // end a quiet remote and one that will never redirect are the same thing.
            Toggle(
                "Enable Audio",
                isOn: Binding(
                    get: { model.audio.isEnabled },
                    set: { model.audio.setEnabled($0) }
                )
            )
            .disabled(!model.audio.isAvailable)

            // The escape hatch for a framebuffer that has gone wrong: re-announce
            // the size and repaint everything.
            Button("Refresh") {
                model.refresh()
            }
            .disabled(model.session.screen != .desktop)

            Button("Switch Target") {
                model.switchTarget()
            }
            .disabled(model.session.screen != .desktop)

            if model.session.connectionStatus == .busy {
                Button("Take Over Session") {
                    model.takeOver()
                }
            } else if model.session.connectionStatus == .takenOver {
                Button("Take Session Back") {
                    model.takeOver()
                }
            }

            Divider()

            // Back to the home screen, to run the local gateway instead of a remote
            // one or the other way round. Also the log out: a remote gateway is told,
            // so the login and the session slot are given up rather than left for the
            // reattach grace period.
            //
            // Named for what it does on both branches. "Log Out" would be wrong on
            // the embedded one, where there is no login to end, and two items that
            // are the same action under two names would be worse than one.
            Button("Change Gateway…") {
                Task { await model.changeGateway() }
            }
            .disabled(!model.canChangeGateway)

            // Where the targets come from. Reachable from every screen the local
            // gateway is behind, including the launch failure — a config it refused is
            // the likeliest reason to be looking for this item at all.
            //
            // Greyed against a remote gateway, which reads its own config wherever it
            // runs: this panel edits the file in *this* instance, and Save restarts a
            // process that session is not using. See `AppModel.canEditConfiguration`.
            //
            // No chord for the same reason nothing else here has one, ⌘, included:
            // while a desktop is focused every Command chord belongs to the guest, so
            // an advertised shortcut would be one that does something else.
            Button("Configuration…") {
                model.editConfiguration()
            }
            .disabled(!model.canEditConfiguration)

            // Restart the gateway this app runs. Not a "log out" — there is no session
            // with the gateway to end — but it is the same escape hatch: everything
            // below the app is rebuilt from the config on disk.
            Button("Restart Local Gateway") {
                Task { await model.relaunchGateway() }
            }
            .disabled(!model.usesEmbeddedGateway || model.isBusy)
        }

        // RXA display selection; other engines leave the stable menu disabled.
        CommandMenu("Display") {
            // A readout rather than a command, and present for every target: on
            // RDP, on VNC, and on an rxa target sharing one of the Mac's own
            // screens, this menu is otherwise empty and these numbers appear
            // nowhere at all. See `displaySummary`.
            Button(
                displaySummary(
                    remote: model.session.remoteSize,
                    remoteScale: model.session.remoteScale,
                    hostScale: model.hostScale
                )
            ) {}
                .disabled(true)
            Divider()

            if model.session.displays.isEmpty {
                Button("No Displays to Choose From") {}
                    .disabled(true)
            } else {
                ForEach(model.session.displays) { display in
                    Toggle(
                        "\(display.label) — \(display.detail)",
                        isOn: Binding(
                            get: { display.id == model.session.activeDisplayID },
                            // Only ever set to true, by picking the item: the
                            // remote is always sharing exactly one display, so
                            // there is no "off" to honour. Unticking the active
                            // item asks for nothing and `selectDisplay` drops it.
                            set: { _ in model.selectDisplay(display.id) }
                        )
                    )
                }
            }
        }
    }
}
