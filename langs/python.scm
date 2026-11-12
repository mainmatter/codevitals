(function_definition name: (identifier) @name parameters: (parameters) @params) @fn
(comment) @comment
[(import_statement) (import_from_statement)] @import
(call function: [(identifier) @name (attribute attribute: (identifier) @name)]) @call
[(if_statement) (elif_clause) (while_statement) (for_statement) (except_clause) (conditional_expression) (case_clause) (if_clause)] @branch
(boolean_operator) @condition
(block) @block
[(global_statement) (nonlocal_statement)] @global_state
(raise_statement) @throw
(class_definition name: (identifier) @name) @scope
(assert_statement) @invariant
(raise_statement [(identifier) @_e (call function: (identifier) @_e)] (#eq? @_e "NotImplementedError")) @unfinished
