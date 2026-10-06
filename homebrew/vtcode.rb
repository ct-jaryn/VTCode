class Vtcode < Formula
  desc "Rust-based terminal coding agent with semantic code intelligence"
  homepage "https://github.com/vinhnx/vtcode"
  license "MIT"
  version "0.172.3"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/vinhnx/vtcode/releases/download/#{version}/vtcode-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "b25c6496a7bb3579d57a553bd0af49c50effb1511ab4997d0e1ba8b5a5202fc6"
    else
      url "https://github.com/vinhnx/vtcode/releases/download/#{version}/vtcode-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "104425b892dcf4c8279b5c53fc6a1c640007b0bcb2569993d44d89cb2ab52977"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/vinhnx/vtcode/releases/download/#{version}/vtcode-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "ab77663ff1efd78bad21ac62a08e8ef885505ea8946895525e958d6e737207b2"
    elsif Hardware::CPU.arm?
      url "https://github.com/vinhnx/vtcode/releases/download/#{version}/vtcode-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "7a5013d45b3bc2d9cbda93f4ac06b115019309f5a65956702ead3b716c1c97ce"
    else
      odie "VT Code #{version} does not support this Linux architecture"
    end
  end

  def install
    bin.install "vtcode"
  end

  def caveats
    <<~EOS
      VT Code is now installed! To get started:

      1. Set your API key environment variable:
         export OPENAI_API_KEY="sk-..."
         (or use ANTHROPIC_API_KEY, GEMINI_API_KEY, etc.)

      2. Launch VT Code:
         vtcode

      Supported providers:
        • OpenAI (OPENAI_API_KEY)
        • Anthropic (ANTHROPIC_API_KEY)
        • Google Gemini (GEMINI_API_KEY)
        • xAI (XAI_API_KEY)
        • DeepSeek (DEEPSEEK_API_KEY)
        • OpenRouter (OPENROUTER_API_KEY)
        • Ollama (local)

      For more information, visit:
        https://github.com/vinhnx/vtcode
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/vtcode --version")
  end
end
