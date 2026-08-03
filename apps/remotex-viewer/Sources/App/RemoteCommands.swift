import AppKit
import SwiftUI

/// The menu bar, which is most of what this app is.
///
/// Every item here stands in for a control the client hides inside this window (see
/// `chromeless` in `frontend/src/RemoteDesktop.tsx`): the same actions a browser
/// puts in a floating drawer, in the place a Mac app puts them. Each one sends a
/// command over the bridge and each one's title, tick and enabled state is read
/// back off what the page reports — so a menu can never claim something the thing
/// on screen is not doing.
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
                    set: { model.setMacKeyOverrides($0) }
                )
            )
            .disabled(!model.canOverrideMacKeys)

            Divider()

            // Opens the client's own clipboard panel, which fetches the remote's
            // text first and shows it concealed. Rebuilding that in AppKit would be
            // two clipboard editors to keep in step, and the consent boundary — the
            // panel's Copy button — would then exist in two places.
            Button("Clipboard…") {
                model.openClipboardPanel()
            }
            .disabled(!model.canClipboard)

            // Sound from the remote. Greyed rather than hidden for a target that has
            // none: a menu whose items come and go is harder to learn than one item
            // that is sometimes disabled.
            //
            // The item says nothing about whether sound is arriving, because from this
            // end a quiet remote and one that will never redirect are the same thing.
            Toggle(
                "Enable Audio",
                isOn: Binding(
                    get: { model.audioEnabled },
                    set: { model.setAudioEnabled($0) }
                )
            )
            .disabled(!model.canAudio)

            // The escape hatch for a framebuffer that has gone wrong: re-announce
            // the size and repaint everything.
            Button("Refresh") {
                model.refresh()
            }
            .disabled(!model.isOnDesktop)

            Button("Switch Target") {
                model.switchTarget()
            }
            .disabled(!model.isOnDesktop)

            // The keys a menu is the only way to send: this app captures the whole
            // keyboard, so ⌥F4 and the modifier taps have nowhere else to come from.
            Menu("Send Keys") {
                ForEach(SpecialKeys.chords, id: \.label) { chord in
                    Button(chord.label) {
                        model.sendKeyCombo(chord.codes)
                    }
                }
                Divider()
                ForEach(SpecialKeys.modifierTaps, id: \.label) { tap in
                    Button("Tap \(tap.label)") {
                        model.sendKeyCombo(tap.codes)
                    }
                }
            }
            .disabled(!model.isOnDesktop)

            if let title = model.takeOverTitle {
                Button(title) {
                    model.takeOver()
                }
            }

            Divider()

            // Where the targets come from. Reachable from every screen, including the
            // launch failure — a config the gateway refused is the likeliest reason to
            // be looking for this item at all.
            //
            // No chord for the same reason nothing else here has one, ⌘, included:
            // while a desktop is focused every Command chord belongs to the guest, so
            // an advertised shortcut would be one that does something else.
            Button("Configuration…") {
                model.editConfiguration()
            }
            .disabled(!model.canEditConfiguration)

            // Restart the gateway this app runs. Everything below the app is rebuilt
            // from the config on disk — including the token, which is why the page is
            // reloaded with it rather than left holding the old one.
            Button("Restart Local Gateway") {
                Task { await model.relaunchGateway() }
            }
            .disabled(!model.canRestartGateway)

            Divider()

            // Chromium's own inspector, on the client. Present in the shipped app
            // rather than a debug build, because the shipped app is the one there
            // is to look at — and because the alternative to reading a console
            // error here is guessing at it from the outside.
            Button("Developer Tools") {
                model.showDevTools()
            }
            .disabled(!model.canShowDevTools)
        }

        CommandMenu("Display") {
            // A readout rather than a command, and present for every target: on
            // RDP and on VNC this menu is otherwise empty and these numbers appear
            // nowhere at all. See `displaySummary`.
            Button(model.displaySummaryLine) {}
                .disabled(true)
            Divider()

            if model.displays.isEmpty {
                Button("No Displays to Choose From") {}
                    .disabled(true)
            } else {
                ForEach(model.displays) { display in
                    Toggle(
                        "\(display.label) — \(display.detail)",
                        isOn: Binding(
                            get: { display.id == model.activeDisplayID },
                            // Only ever set to true, by picking the item: the
                            // remote is always sharing exactly one display, so
                            // there is no "off" to honour.
                            set: { _ in model.selectDisplay(display.id) }
                        )
                    )
                }
            }
        }
    }
}

/// The chords a captured keyboard cannot type.
///
/// Two kinds, and they are different things. The first are chords macOS keeps for
/// itself or spells differently — ⌥F4 has no Mac equivalent at all. The second are
/// single modifier *taps*, which a guest reads as "open the menu" and which cannot
/// be typed here because holding a modifier and letting go is a chord with nothing
/// in it as far as this Mac's keyboard is concerned.
enum SpecialKeys {
    struct Chord {
        let label: String
        let codes: [String]
    }

    static let chords: [Chord] = [
        Chord(label: "F5", codes: ["F5"]),
        Chord(label: "F11", codes: ["F11"]),
        Chord(label: "Ctrl+R", codes: ["ControlLeft", "KeyR"]),
        Chord(label: "Ctrl+W", codes: ["ControlLeft", "KeyW"]),
        Chord(label: "Ctrl+T", codes: ["ControlLeft", "KeyT"]),
        Chord(label: "Alt+F4", codes: ["AltLeft", "F4"]),
    ]

    static let modifierTaps: [Chord] = [
        Chord(label: "Ctrl", codes: ["ControlLeft"]),
        Chord(label: "Alt", codes: ["AltLeft"]),
        Chord(label: "Shift", codes: ["ShiftLeft"]),
        Chord(label: "Super", codes: ["MetaLeft"]),
    ]
}
