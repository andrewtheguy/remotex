import RemoteDesktop from "./RemoteDesktop.tsx";

export default function App() {
  return (
    <div className="app">
      <header className="app-header">
        <h1>rdpweb</h1>
      </header>
      <main className="app-main">
        <RemoteDesktop />
      </main>
    </div>
  );
}
