(function_definition) @fn
(function_definition declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @params)) @fn
(function_definition declarator: (pointer_declarator declarator: (function_declarator declarator: (identifier) @name parameters: (parameter_list) @params))) @fn
(comment) @comment
(preproc_include path: (_) @name) @import
(call_expression function: (identifier) @name) @call
(function_definition (storage_class_specifier) @_s (#eq? @_s "static")) @static
