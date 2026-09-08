class Remotex < Formula
  desc "Single-user browser remote desktop gateway"
  homepage "https://github.com/andrewtheguy/remotex"
  url "https://github.com/andrewtheguy/remotex/releases/download/v0.0.194/remotex-0.0.194-macos-arm64.tar.gz"
  sha256 "130edb17449c30d0ce1d97f85a1317bf7523c443fffa4929947915bcef62e6b9"

  livecheck do
    url :homepage
    strategy :github_latest
  end

  depends_on arch: :arm64
  depends_on :macos

  def install
    bin.install "bin/remotex"
    (pkgshare/"web").install Dir["share/remotex/web/*"]
    doc.install "share/doc/remotex/remotex.example.toml"
  end

  post_install_steps do
    unless_path_exists "remotex/remotex.toml", base: :etc do
      mkdir_p "remotex", base: :etc
      copy "share/doc/remotex/remotex.example.toml", "remotex/remotex.toml",
           source_base: :prefix, target_base: :etc, overwrite: false
      set_permissions "remotex/remotex.toml", "0600", base: :etc, recursive: false
    end
  end

  service do
    run [opt_bin/"remotex", "serve", "--config", etc/"remotex/remotex.toml"]
    keep_alive successful_exit: false
    process_type :standard
    log_path var/"log/remotex/stdout.log"
    error_log_path var/"log/remotex/stderr.log"
  end

  def caveats
    <<~EOS
      Edit the starter config before starting the gateway:
        #{etc}/remotex/remotex.toml

      Generate the web login credential with:
        remotex gen-passwd admin

      Then start the non-TUI gateway at login with:
        brew services start andrewtheguy/remotex/remotex
    EOS
  end

  test do
    assert_match "remotex #{version}", shell_output("#{bin}/remotex --version")
  end
end
