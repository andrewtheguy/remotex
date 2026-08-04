#!/usr/bin/env node
// A config the gateway will not serve: it says why on stderr and exits, printing
// no handshake at all.
process.stderr.write(
  "error: this config is remotex.app's own and may not have a [server] block\n",
);
process.exit(1);
