;; Cargo.toml: the crate's names, [package] name and [lib] name (what `use` writes).
(table (bare_key) @_table (#any-of? @_table "package" "lib")
  (pair (bare_key) @_key (#eq? @_key "name") (string) @name) @package)
