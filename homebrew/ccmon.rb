class Ccmon < Formula
  desc "List running Claude Code agents & subagents, open a viewer in a new terminal tab"
  homepage "https://github.com/adamclark64/ccmon"
  version "0.1.0"
  license "MIT"

  depends_on "jq"
  depends_on "tmux"

  on_macos do
    on_arm do
      url "https://github.com/adamclark64/ccmon/releases/download/v0.1.0/ccmon-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "8ae1bee00745b6de446aac4d49ae491ac824eb6253083dae6e69100367eb56c2"
    end
    on_intel do
      url "https://github.com/adamclark64/ccmon/releases/download/v0.1.0/ccmon-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "bcc8175dafc24f02e0e6cbb315bc7b21059a34cc92b13eb5fbf19dcb82c7f27d"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/adamclark64/ccmon/releases/download/v0.1.0/ccmon-0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "1d7e69b391c5a6806028788b329206098367ba43e265f40c123aef20eed7268b"
    end
    on_intel do
      url "https://github.com/adamclark64/ccmon/releases/download/v0.1.0/ccmon-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "4a99216338640ca6273dff3de966f3374c47edeaa0a1c706a1863ece44513ef2"
    end
  end

  def install
    bin.install "ccmon"
  end

  test do
    system bin/"ccmon", "--list"
  end
end
