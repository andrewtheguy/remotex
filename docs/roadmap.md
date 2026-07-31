# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### macOS login-window and unattended access

The gateway reaches a Mac through its built-in Screen Sharing over VNC, in the
signed-in user's Aqua session. So the desktop cannot be reached from the macOS
login screen, and if the screen is locked the user must unlock it at the Mac
before remotex can connect.

The shape of an answer is established: RealVNC's
[Service Mode](https://help.realvnc.com/hc/en-us/articles/360002253238-Understanding-RealVNC-Server-Modes)
and RustDesk's installed-service mode both provide login-window access, by
installing launch components that declare the `LoginWindow` session type
alongside `Aqua`, rather than running only in a per-user Aqua session.

Nothing past that shape is settled, and none of it should be designed here first.
What has to be measured on a real Mac: how the single listener is held across the
login transition and fast user switching, where a configuration readable by both
the configured user and the UID 0 login-window process should live, and whether
Screen Recording and Accessibility grants reach the service in the
`LoginWindow` session at all.

FileVault is the one boundary none of it crosses: no remote-access process runs
before pre-boot disk unlock.

### Phase 2 — Apple Screen Sharing display picking and high performance

macOS Screen Sharing can natively pick a single display: the stock Screen Sharing
app shows a Both Displays / Display 1 / Display 2 choice. The gateway does not do
this yet. Today it shares the Mac's real screen(s) as-is over standard screen
sharing; teaching the VNC/ARD path to enumerate the Mac's displays and bind to one
is phase 2. The `ClientMsg::SelectDisplay` / `ServerMsg::Displays` wire is kept as
scaffolding for exactly that, and `src/vnc.rs` currently returns an empty display
list.

"High-performance" screen sharing goes one step further: it spins up a resizable
virtual display and allows dynamic resize the way RDP does. That is where `resize`
on an `ard` target becomes real — it is rejected at configuration time today.

### A virtual-display-only macOS utility (deferred, low priority)

BetterDisplay already covers the need, so this is revisited only if more control is
required. A small app that creates a `CGVirtualDisplay` at a chosen size — the mold
BetterDisplay is cut from — would let macOS Screen Sharing share that display over
plain ARD with no bespoke code on either side. The mechanism is salvageable from git
history at commit `8990971` (`crates/rxa-agent/src/virtualdisplay.rs` and the
`virtual_display*` config fields).

### RDP resize requested before the Display Control channel is ready

A client-initiated resize on RDP travels as a `Viewport` (or `DefaultSize`)
message, and the gateway answers it over the Display Control dynamic channel —
which is not open the instant the session connects. A request that arrives in that
window returns `Asked::NotReady` from `request_layout`, and the `Viewport` arm in
`src/rdp.rs` **drops that outcome**: unlike a density change, which schedules a
retry (`pending_density` / `density_retry_at`), a size that could not be sent is
never re-sent, because a client states its viewport once and dedupes it.

This is invisible until something reports a viewport *at connect*, which is exactly
what auto-resize-by-default does: with the preference on, both clients report their
window from the `connected` handshake, before the channel is up, so on RDP the
desktop stays at its connect size until the next window change lands after the
channel has opened. Toggling Auto Resize off and on, or nudging the window, applies
it. VNC has no such gate — its engine acts on any viewport report whenever it
arrives.

The fix is to retry a `NotReady` size the way density is retried, but **serialized
with the density retry, not alongside it**: two independent layout retries racing
would each drive a Deactivation-Reactivation and desync `applied` from the desktop
actually negotiated — the invariant the `DeactivateAll` arm and the note by
`applied` exist to protect. The cleaner shape is a single pending "wanted layout"
(size and density together) with one retry by construction, so the last request
wins and there is nothing to serialize. Not attempted yet; the choice between the
two is the part to settle on a real Windows host, where the channel-open timing and
the reactivation behaviour can be measured rather than guessed.

## Not planned

### Multiple sessions

**Concurrent sessions, shared sessions, and a session broker are outside the
product model.** This is one user's program, and that is not a limitation waiting
to be lifted.

There is one active session slot: one active session per gateway instance,
permanently. A new browser takes over and evicts the previous holder
(`src/session.rs`), which a client offers with a Take over button — the same
shape as Windows Remote Desktop. A reconnect, a target switch and a browser
takeover all reclaim the slot in silence: they are the same session coming back,
whatever else has changed.
