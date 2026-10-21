# Read-only evaluation harness, not Homebrew and never an installer.
# Faithful narrow model of MacOSRequirement.parse/from_symbol: only the named
# symbols emitted by the generator are accepted, never numeric comparator text.
class Version
  include Comparable
  attr_reader :parts
  def initialize(text)
    raise "unsafe version" unless /\A\d+(?:\.\d+){0,2}\z/.match?(text)
    @parts = (text.split(".").map(&:to_i) + [0, 0]).take(3)
  end
  def <=>(other)
    parts <=> other.parts
  end
end
module MacOS
  def self.full_version
    Version.new(ENV.fetch("CASK_TEST_MACOS"))
  end
end
class CaskDSLFixture
  SYMBOLS = {big_sur: "11", monterey: "12", ventura: "13", sonoma: "14",
             sequoia: "15", tahoe: "26", golden_gate: "27"}.freeze
  [:version, :sha256, :url, :name, :desc, :homepage, :app, :caveats].each do |method|
    define_method(method) { |_value| }
  end
  def depends_on(values)
    if values.key?(:macos)
      raise "Homebrew macOS dependency must be a supported Symbol" unless SYMBOLS.key?(values[:macos])
    elsif values != {arch: :arm64}
      raise "unexpected dependency"
    end
  end
  def preflight(&block)
    @guard = block
  end
  def verify_guard
    raise "missing exact minimum preflight" unless @guard
    instance_eval(&@guard)
  end
end
def cask(token, &block)
  raise "unexpected token" unless token == "gliff-macos"
  fixture = CaskDSLFixture.new
  fixture.instance_eval(&block)
  fixture.verify_guard
end
load ARGV.fetch(0)
