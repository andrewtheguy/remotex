#!/usr/bin/env node
// Ignores SIGTERM. Only the pipe closing — or SIGKILL after it — can stop this one,
// which is exactly why the pipe is the layer the guarantee rests on.
process.on("SIGTERM", () => {});
process.stderr.write("started\n");
process.stdout.write(`${JSON.stringify({ port: 5555, token: "t" })}\n`);
process.stdin.on("end", () => process.exit(0));
process.stdin.resume();
