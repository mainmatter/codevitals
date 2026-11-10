(function_item name: (identifier) @name parameters: (parameters) @params) @fn
[(line_comment) (block_comment)] @comment
(use_declaration argument: (_) @name) @import
(call_expression function: [(identifier) @name (scoped_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name) (generic_function function: [(identifier) @name (scoped_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name)])]) @call
(function_item (visibility_modifier)) @pub
