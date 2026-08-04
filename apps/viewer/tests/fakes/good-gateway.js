#!/usr/bin/env node
// A gateway that behaves: one handshake line on stdout, its reasons on stderr, and
// an exit when the pipe closes. The last part is the contract that matters — it is
// what `src/embedded.rs` promises and what the app's "no stray gateway" guarantee
// rests on.
process.stderr.write("listening on 127.0.0.1:49213\n");
process.stdout.write(`${JSON.stringify({ port: 49213, token: "a-token" })}\n`);
process.stdin.on("end", () => process.exit(0));
process.stdin.resume();
