// The build, as every screen that shows it spells it: the bundle's version, then
// the optional features of the gateway serving it.
//
// Two sources because they are two facts. The version is compiled into the bundle;
// the features belong to the binary, and one bundle is compiled into every build of
// it, so only the gateway can say which it is. A gateway built with none adds nothing.

import { useEffect, useState } from "react";
import { gatewayConfig } from "./gatewayConfig.ts";

export function AppVersion({ className }: { className: string }) {
  const [features, setFeatures] = useState<string[]>([]);

  useEffect(() => {
    let cancelled = false;
    gatewayConfig().then((config) => {
      if (!cancelled) {
        setFeatures(config.features);
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className={className}>
      v{__APP_VERSION__}
      {features.length > 0 && ` (${features.join(", ")})`}
    </div>
  );
}
