(function_declaration name: (identifier) @name parameters: (formal_parameters) @params) @fn
(generator_function_declaration name: (identifier) @name parameters: (formal_parameters) @params) @fn
(method_definition name: (_) @name parameters: (formal_parameters) @params) @fn
(variable_declarator name: (identifier) @name value: (arrow_function)) @fn
(variable_declarator name: (identifier) @name value: (arrow_function parameters: (formal_parameters) @params)) @fn
(variable_declarator name: (identifier) @name value: (function_expression parameters: (formal_parameters) @params)) @fn
(comment) @comment
(import_statement source: (string) @name) @import
(call_expression function: [(identifier) @name (member_expression property: (property_identifier) @name)]) @call
(export_statement declaration: (function_declaration) @pub)
(export_statement declaration: (generator_function_declaration) @pub)
(export_statement declaration: (lexical_declaration (variable_declarator value: [(arrow_function) (function_expression)]) @pub))
