# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### macOS login-window service

The agent's LaunchAgent is per-user and runs only in the signed-in user's Aqua
session, so the agent stops at logout and cannot be reached from the macOS login
screen. Currently if the screen is locked, the user must unlock it before remotex can connect. 

The shape of an answer is established: RealVNC's
[Service Mode](https://help.realvnc.com/hc/en-us/articles/360002253238-Understanding-RealVNC-Server-Modes)
and RustDesk's installed-service mode both provide login-window access, by
installing launch components that declare the `LoginWindow` session type
alongside `Aqua`, rather than a per-user agent in `~/Library/LaunchAgents`.

Nothing past that shape is settled, and none of it should be designed here first.
What has to be measured on a real Mac: how the single listener is held across the
login transition and fast user switching, where a config and private key readable by both
the configured user and the UID 0 login-window process should live, and whether
Screen Recording and Accessibility grants reach the signed app in the
`LoginWindow` session at all.

FileVault is the one boundary none of it crosses: no remote-access process runs
before pre-boot disk unlock.

### Input fidelity on macOS

Injecting input is not the same as having input, and the gap is made of fields
and timings that no amount of looking at the screen reveals — a click state that
is never set looks exactly like a click. These are the ones known to be missing,
found by reading RustDesk's macOS injection path (`libs/enigo/src/macos/macos_impl.rs`
and `src/server/input_service.rs` in that project), which is the closest thing to
a reference implementation of the same job.

None is known to have bitten anyone here yet. They are recorded because the cost
of finding each one from a bug report is high and the cost of writing it down is
not.

**Relative pointer motion has no path of its own.** A move now carries
`kCGMouseEventDeltaX/Y` alongside where it landed, which is what an app reading
relative input sees, so a pointer-locked game or 3D viewer is no longer looking at
a motionless mouse. What is still missing is the mode: the browser's Pointer Lock
API on the client, a wire message carrying a delta rather than a position, and a
per-session flag saying which of the two a client is sending. Without it the
pointer still walks to the edge of the remote screen and stops. RustDesk models
this as a separate `MOUSE_TYPE_MOVE_RELATIVE` message with the absolute one, which
is the shape to copy if it is ever wanted.

**CapsLock is expressed as a flag, not as the lock.** Every injected event carries
`MaskAlphaShift` when the client says CapsLock is on
(`crates/rxa-agent/src/input.rs`), which produces the right characters but leaves
the Mac's own lock state untouched. RustDesk instead presses and releases the real
CapsLock key so the two agree. The difference shows if the Mac's physical CapsLock
is on while the client's is off: the remote's lock applies on top of our flag, and
nothing we send can turn it off. Worth revisiting if anyone reports case that will
not go away.

**Key events are posted with no pacing.** RustDesk sleeps 12 ms after every key
event on macOS, having found that a Shift release can otherwise fail to take
effect and leave the remote typing uppercase. We sleep nothing. It may not apply:
they post keys at `CGEventTapLocation::Session` where we use `HID`, and their own
comment says HID fixes a related Command-key bug they chose not to move for. If
sticky modifiers ever appear under fast typing, this is the first thing to try.

**The side buttons stop at the rxa engine.** Back and forward reach a Mac
(`MouseButton::Back`/`Forward`), and RDP and VNC drop them: RDP carries them in an
extended pointer PDU the fast-path event cannot express, and RFB's mask has bits
for them that no server remotex talks to agrees on. Both would need protocol work
rather than a mapping, which is why neither was attempted.

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
it. VNC and rxa have no such gate — their engines act on any viewport report
whenever it arrives.

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

There are two session slots, and they are separate mechanisms answering the same
question at different hops:

- **The gateway's.** One active session per gateway instance, permanently. A new
  browser takes over and evicts the previous holder (`src/session.rs`).
- **The agent's.** One active session at a time on a Mac running `remotex-agent`,
  claimed rather than seized: a connection asks with `GatewayMsg::Claim`, and the
  agent grants it, hands it over, or refuses it and names who holds it
  (`crates/rxa-agent/src/state.rs`). A client shows that refusal with a Take over
  button, which is the same shape as the browser prompt above and the same shape
  as Windows Remote Desktop.

What the agent's slot is keyed on is worth stating plainly, because getting it
wrong is what would collapse the distinction: **the session id in the claim, and
never a key or an address.** Authentication decides whether a peer may ask at all
— that is the keys, in the handshake, and it is a *list* there
(`crates/rxa-agent/src/authorized.rs`), so several gateways can be entitled to
reach one Mac. Session ownership decides whose turn it is. Keeping them apart is
what lets several gateways be *permitted* while exactly one is *connected*, and it
is also why a reconnect, a target switch and a browser takeover all reclaim the
slot in silence: they are the same session coming back, whatever else has changed.

So "more than one client may be permitted, one at a time, taking turns
explicitly" is a different sentence from "multiple sessions", and only the second
one is refused here.
