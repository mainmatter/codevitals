(function_item name: (identifier) @name parameters: (parameters) @params) @fn
[(line_comment) (block_comment)] @comment
(use_declaration) @import
(mod_item name: (identifier) @name body: (declaration_list)) @module
(call_expression function: [(identifier) @name (scoped_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name) (generic_function function: [(identifier) @name (scoped_identifier name: (identifier) @name) (field_expression field: (field_identifier) @name)])]) @call
(function_item (visibility_modifier)) @pub
[(if_expression) (while_expression) (for_expression) (match_arm)] @branch
(binary_expression operator: ["&&" "||"]) @condition
(block) @block
(static_item (mutable_specifier)) @global_state
(macro_invocation macro: (identifier) @_m (#eq? @_m "panic")) @throw
(macro_invocation macro: (identifier) @name (#match? @name "^(debug_)?assert(_eq|_ne)?$|^unreachable$")) @invariant
(macro_invocation macro: (identifier) @_m (#match? @_m "^(todo|unimplemented)$")) @unfinished
(call_expression function: (field_expression field: (field_identifier) @_f) (#match? @_f "^(unwrap|expect)$")) @unhandled
(mod_item name: (identifier) @name !body) @mod_decl
[(attribute_item) (inner_attribute_item)] @attr
(impl_item type: (_) @name) @scope
(trait_item name: (_) @name) @scope
