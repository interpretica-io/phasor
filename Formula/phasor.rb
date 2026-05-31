class Phasor < Formula
  desc "Dashboard to monitor and orchestrate AI coding agents in tmux"
  homepage "https://github.com/interpretica-io/phasor"
  # Stable release. After tagging `vX.Y.Z`, point `url` at that tag and set
  # `sha256` to the source tarball's checksum — `brew fetch phasor` prints it,
  # or run `shasum -a 256` on the downloaded tarball.
  url "https://github.com/interpretica-io/phasor/archive/refs/tags/v0.4.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/interpretica-io/phasor.git", branch: "main"

  depends_on "rust" => :build
  depends_on "tmux"

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      phasor drives the Claude Code CLI (`claude`), which is not available via
      Homebrew. Install it separately and make sure it is on your PATH.
    EOS
  end

  test do
    # An unknown subcommand prints usage and exits 2 (no TTY/tmux server needed).
    output = shell_output("#{bin}/phasor not-a-command 2>&1", 2)
    assert_match "usage", output
  end
end
