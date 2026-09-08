#!/usr/bin/env ruby
# Update the release-bound fields in Formula/remotex.rb after the macOS tarball
# has been published.

unless (2..3).cover?(ARGV.length)
  abort "usage: #{$PROGRAM_NAME} <version> <sha256> [formula]"
end

version, sha256, requested_path = ARGV
unless version.match?(/\A\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?\z/)
  abort "invalid version: #{version.inspect}"
end
unless sha256.match?(/\A[0-9a-f]{64}\z/)
  abort "invalid SHA-256: #{sha256.inspect}"
end

path = requested_path || File.expand_path("../Formula/remotex.rb", __dir__)
text = File.read(path)

release_url = %r{^  url "https://github\.com/andrewtheguy/remotex/releases/download/v[^/]+/remotex-[^"]+-macos-arm64\.tar\.gz"$}
url_lines = text.scan(release_url).length
sha_lines = text.scan(/^  sha256 "[0-9a-f]{64}"$/).length
abort "expected one release URL line in #{path}, found #{url_lines}" unless url_lines == 1
abort "expected one SHA-256 line in #{path}, found #{sha_lines}" unless sha_lines == 1

text.sub!(release_url,
          %(  url "https://github.com/andrewtheguy/remotex/releases/download/v#{version}/remotex-#{version}-macos-arm64.tar.gz"))
text.sub!(/^  sha256 "[0-9a-f]{64}"$/, %(  sha256 "#{sha256}"))
File.write(path, text)
