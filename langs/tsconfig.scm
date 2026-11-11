;; tsconfig.json and jsconfig.json: compilerOptions.paths, each alias (`@/*`) as an
;; `alias` span holding an `alias_target` span per target (`./src/*`), and
;; compilerOptions.baseUrl, which targets are relative to.
(document (object (pair key: (string (string_content) @_options) (#eq? @_options "compilerOptions")
  value: (object (pair key: (string (string_content) @_paths) (#eq? @_paths "paths")
    value: (object (pair key: (string (string_content) @name) value: (array)) @alias))))))
(document (object (pair key: (string (string_content) @_options) (#eq? @_options "compilerOptions")
  value: (object (pair key: (string (string_content) @_paths) (#eq? @_paths "paths")
    value: (object (pair value: (array (string (string_content) @name) @alias_target))))))))
(document (object (pair key: (string (string_content) @_options) (#eq? @_options "compilerOptions")
  value: (object (pair key: (string (string_content) @_base) (#eq? @_base "baseUrl")
    value: (string (string_content) @name)) @base_url))))
