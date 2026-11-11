# frozen_string_literal: true

require "minitest/autorun"
Warning[:experimental] = false

$LOAD_PATH.unshift(File.expand_path("../lib", __dir__))
require "rubydex"

module Minitest
  class Test
    # The disk-index suites assert behaviour of a store that only exists when the feature was
    # compiled in; a build without it has no store to test, only the in-memory fallback.
    def skip_unless_store_feature
      skip("built without the redb-store feature") unless Rubydex::Graph.store_format_version.positive?
    end
  end
end

# A hung test otherwise costs the whole job (six hours on Windows before GitHub cancels it) and
# leaves no clue which test hung it: minitest only writes a test's name after it finishes.
unless ENV["RUBYDEX_TEST_TRACE"].to_s.empty?
  $stdout.sync = true

  module RubydexTestTrace
    def run
      puts("#rubydex test: #{self.class}##{name}")
      super
    end
  end

  Minitest::Test.prepend(RubydexTestTrace)
end
