// The version, as every screen that shows it spells it: the bundle's version, then
// `(airplay)` when the gateway serving it runs a Mac's AirPlay speaker.
//
// Two sources because they are two facts. The version is compiled into the bundle;
// the speaker is the gateway's `[airplay]` table, which only the gateway can report.

import { useEffect, useState } from "react";
import { gatewayConfig } from "./gatewayConfig.ts";

export function AppVersion({ className }: { className: string }) {
  const [airplay, setAirplay] = useState(false);

  useEffect(() => {
    let cancelled = false;
    gatewayConfig().then((config) => {
      if (!cancelled) {
        setAirplay(config.airplay);
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className={className}>
      v{__APP_VERSION__}
      {airplay && " (airplay)"}
    </div>
  );
}
