# frozen_string_literal: true

# The release workflow copies this file into the tap with the @…@
# placeholders filled in from the release.
class Carte < Formula
  desc "Runs your apps' processes on demand behind *.test hostnames"
  homepage "https://github.com/elektronaut/carte"
  url "https://github.com/elektronaut/carte/archive/refs/tags/v@VERSION@.tar.gz"
  sha256 "@SOURCE_SHA256@"
  license "MIT"

  livecheck do
    url :stable
    strategy :github_latest
  end

  depends_on :macos

  resource "binary" do
    on_arm do
      url "https://github.com/elektronaut/carte/releases/download/v@VERSION@/carte-@VERSION@-aarch64-apple-darwin.tar.gz"
      sha256 "@ARM64_SHA256@"
    end
    on_intel do
      url "https://github.com/elektronaut/carte/releases/download/v@VERSION@/carte-@VERSION@-x86_64-apple-darwin.tar.gz"
      sha256 "@X86_64_SHA256@"
    end
  end

  def install
    resource("binary").stage { bin.install "carte" }
  end

  def caveats
    <<~EOS
      Run `carte setup` once to install the daemon, DNS resolver and local CA.
      It pins #{opt_bin}/carte, so upgrades keep working.
    EOS
  end

  test do
    assert_match "carte #{version}", shell_output("#{bin}/carte --version")
  end
end
