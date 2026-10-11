; Rust mod test
((attribute_item
  (attribute
    (identifier) @_attribute
    arguments: ((token_tree
      (identifier) @_test)
      (#eq? @_test "test")))
  (#eq? @_attribute "cfg"))
  .
  (mod_item
    name: (_) @run)
  (#set! tag rust-mod-test))

; Rust test
((
  ((attribute_item) @_attribute_item @run_item)+
  .
  [
    (line_comment)
    (block_comment)
  ]*
  .
  (function_item
    name: (_) @run @_test_name
    body: _) @_end)
  (#set! tag rust-test))

; Rust doc test
((
  [
    (line_comment) @run @run_item
    (attribute_item)
    (block_comment)
  ]+
  .
  [
    (function_item
      name: (_) @_doc_test_name
      body: _)
    (function_signature_item
      name: (_) @_doc_test_name)
    (struct_item
      name: (_) @_doc_test_name)
    (enum_item
      name: (_) @_doc_test_name
      body: _)
    (macro_definition
      name: (_) @_doc_test_name)
    (mod_item
      name: (_) @_doc_test_name)
  ] @_end)
  (#set! tag rust-doc-test))

; Rust main function
(((function_item
  name: (_) @run
  body: _) @_rust_main_function_end
  (#eq? @run "main"))
  (#set! tag rust-main))
