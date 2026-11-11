;; package.json: the package's name, as imports write it (`@scope/pkg`).
(document (object
  (pair key: (string (string_content) @_key) (#eq? @_key "name")
        value: (string (string_content) @name)) @package))
