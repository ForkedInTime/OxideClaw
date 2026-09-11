# Homebrew formula for RustyClaw. Lives in the tap
# github.com/ForkedInTime/homebrew-rustyclaw; kept here as the source of
# truth and copied there on each release.
class Rustyclaw < Formula
  desc "Single-binary coding agent: codebase index, cost-aware routing, worktree agents, voice"
  homepage "https://github.com/ForkedInTime/RustyClaw"
  version "0.3.2"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/ForkedInTime/RustyClaw/releases/download/v#{version}/rustyclaw-macos-arm64"
      sha256 "d36c4206c52f2a115bf66127b155126dc9d574fb57fec7ce7fccb1ec09aeecc6"
    end
    on_intel do
      url "https://github.com/ForkedInTime/RustyClaw/releases/download/v#{version}/rustyclaw-macos-x64"
      sha256 "404d0733163a5f6fa17d8a22f228eba48605d0ad26007dd6becb433822785dfd"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ForkedInTime/RustyClaw/releases/download/v#{version}/rustyclaw-linux-arm64"
      sha256 "8584bf8a6a4ec36e1dc9fa5989072b2fe5ac91257ff623c576bf1cc7cdee29d2"
    end
    on_intel do
      url "https://github.com/ForkedInTime/RustyClaw/releases/download/v#{version}/rustyclaw-linux-x64"
      sha256 "52155bc9832dc4440cc20fbc6fd25dd417edda653a5a6cbb2e5241d5952d856c"
    end
  end

  def install
    binary = Dir["rustyclaw-*"].first
    bin.install binary => "rustyclaw"
    generate_completions_from_executable(bin/"rustyclaw", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/rustyclaw version")
  end
end
