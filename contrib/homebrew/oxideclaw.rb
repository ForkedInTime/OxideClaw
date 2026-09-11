# Homebrew formula for OxideClaw. Lives in the tap
# github.com/ForkedInTime/homebrew-oxideclaw; kept here as the source of
# truth and copied there on each release.
class Oxideclaw < Formula
  desc "Single-binary coding agent: codebase index, cost-aware routing, worktree agents, voice"
  homepage "https://github.com/ForkedInTime/OxideClaw"
  version "0.4.0"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/ForkedInTime/OxideClaw/releases/download/v#{version}/oxideclaw-macos-arm64"
      sha256 "926881aa989ed1d57602697cd91a380166cc84a8d05d96173d5ff009fb60e0cf"
    end
    on_intel do
      url "https://github.com/ForkedInTime/OxideClaw/releases/download/v#{version}/oxideclaw-macos-x64"
      sha256 "ea8176956f70c4846b04bc852bc7b896fe61c4cbe8f0967eb294a99df83944bf"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ForkedInTime/OxideClaw/releases/download/v#{version}/oxideclaw-linux-arm64"
      sha256 "c4f6d018c2f9c5244a0be6f70cf33a7027a29f4ca147adf8a4bd0fa0d2d65d1a"
    end
    on_intel do
      url "https://github.com/ForkedInTime/OxideClaw/releases/download/v#{version}/oxideclaw-linux-x64"
      sha256 "b6cb388873f0b83ffa405cab7c14a36647c9488eb3009f8ab55e7ce346f87bd8"
    end
  end

  def install
    binary = Dir["oxideclaw-*"].first
    bin.install binary => "oxideclaw"
    generate_completions_from_executable(bin/"oxideclaw", "completions")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/oxideclaw version")
  end
end
