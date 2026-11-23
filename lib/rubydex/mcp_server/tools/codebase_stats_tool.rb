# frozen_string_literal: true

module Rubydex
  module MCPServer
    class CodebaseStatsTool < BaseTool
      tool_name "codebase_stats"
      description "Count indexed files, declarations, definitions, and references, with declarations grouped by kind."
      input_schema(properties: {})

      #: -> Tool::Response
      def call
        declaration_count = 0
        breakdown = Hash.new(0)
        @graph.declarations.each do |declaration|
          declaration_count += 1
          breakdown[declaration_kind(declaration)] += 1
        end

        # `.size`, not `.count`: the enumerators know their length, while counting walks them and
        # decodes every definition and reference of the store to produce numbers already on hand.
        response(
          files: @graph.documents.count,
          declarations: declaration_count,
          definitions: @graph.documents.sum { |document| document.definitions.size },
          constant_references: @graph.constant_references.size,
          method_references: @graph.method_references.size,
          breakdown_by_kind: breakdown,
        )
      end
    end
  end
end
