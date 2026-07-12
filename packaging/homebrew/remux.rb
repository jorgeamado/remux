# Homebrew formula for remux. Lives in the tap repository
# (github.com/jorgeamado/homebrew-remux as Formula/remux.rb); this copy is the
# template release CI fills in. Users then install with:
#
#   brew tap jorgeamado/remux
#   brew install remux
#
class Remux < Formula
  desc "Your persistent tmux session, on your phone"
  homepage "https://github.com/jorgeamado/remux"
  version "{{VERSION}}"
  license "MIT"

  depends_on "tmux"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/jorgeamado/remux/releases/download/v{{VERSION}}/remux-v{{VERSION}}-aarch64-apple-darwin.tar.gz"
      sha256 "{{SHA_MACOS_ARM64}}"
    else
      url "https://github.com/jorgeamado/remux/releases/download/v{{VERSION}}/remux-v{{VERSION}}-x86_64-apple-darwin.tar.gz"
      sha256 "{{SHA_MACOS_X86_64}}"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/jorgeamado/remux/releases/download/v{{VERSION}}/remux-v{{VERSION}}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA_LINUX_ARM64}}"
    else
      url "https://github.com/jorgeamado/remux/releases/download/v{{VERSION}}/remux-v{{VERSION}}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "{{SHA_LINUX_X86_64}}"
    end
  end

  def install
    bin.install "remux"
  end

  def caveats
    <<~EOS
      Start the daemon on your tailnet interface (never a public one):
        remux serve --listen <tailscale-ip>:7777
      It prints a single-use pairing QR/link for your phone. See the README
      for TLS via `tailscale cert` (required to install the PWA on iOS).
    EOS
  end

  test do
    assert_match "remux", shell_output("#{bin}/remux --version")
  end
end
