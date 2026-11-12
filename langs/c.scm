(function_definition) @fn
(function_definition declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @params)) @fn
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @params))) @fn
(comment) @comment
(preproc_include) @import
(call_expression function: (identifier) @name) @call
(function_definition (storage_class_specifier) @_s (#eq? @_s "static")) @static
[(if_statement) (while_statement) (for_statement) (do_statement) (case_statement) (conditional_expression)] @branch
(binary_expression operator: ["&&" "||"]) @condition
(compound_statement) @block
(translation_unit (declaration declarator: [(identifier) (init_declarator) (array_declarator) (pointer_declarator declarator: (identifier))]) @global_state (#not-match? @global_state "^(extern|const|static const)[ \t\n]"))
(goto_statement) @goto
(call_expression function: (identifier) @_f (#match? @_f "^(longjmp|siglongjmp)$")) @throw
(call_expression function: (identifier) @_a (#eq? @_a "assert")) @invariant
