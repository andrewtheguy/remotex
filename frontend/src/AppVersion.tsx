// The version, as every screen that shows it spells it: the bundle's version,
// compiled into the bundle.

export function AppVersion({ className }: { className: string }) {
  return <div className={className}>v{__APP_VERSION__}</div>;
}
