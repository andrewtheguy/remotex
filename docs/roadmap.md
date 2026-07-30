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

### Authorized gateway list

The agent holds one `gateway_public_key` and answers that gateway alone, so
"one active session" and "one permitted gateway" are currently the same fact and
nothing in its config distinguishes them. They are different questions, and the
second one has a better answer: a list, the way `~/.ssh/authorized_keys` is a
list, with a comment against each entry naming the machine it belongs to.

What it buys is a Mac reachable from more than one gateway — a home server and a
laptop, say — without re-pairing it each time, and an agent that can say *which*
gateway is watching rather than only that somebody is.

It does not touch the session rule below: one gateway holds the agent at a time,
and a second has to take it over. That part is already built, and deliberately
built first, keyed on the session id in `GatewayMsg::Claim` rather than on any
key — so adding a second permitted key cannot turn into a second concurrent
session by accident.

The shape is settled enough to name and not settled enough to build here: the
handshake becomes `Noise_IK` so the agent learns which key dialed and can look it
up, which is a protocol version change, and the Settings dialog's single key
field becomes a button onto the list.

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
— that is the keys, in the handshake, and it is the layer the item above extends
to a list. Session ownership decides whose turn it is. Keeping them apart is what
lets several gateways be *permitted* while exactly one is *connected*, and it is
also why a reconnect, a target switch and a browser takeover all reclaim the slot
in silence: they are the same session coming back, whatever else has changed.

So "more than one client may be permitted, one at a time, taking turns
explicitly" is a different sentence from "multiple sessions", and only the second
one is refused here.
