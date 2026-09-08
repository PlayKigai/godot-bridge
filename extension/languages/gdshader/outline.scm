(function_declaration
  name: (_) @name) @item

(struct_declaration
  "struct" @context
  name: (_) @name) @item

(const_declaration
  "const" @context
  specifier: (var_specifier name: (_) @name)) @item

(varying_declaration
  "varying" @context
  specifier: (var_specifier name: (_) @name)) @item

(uniform_declaration
  "uniform" @context
  specifier: (var_specifier name: (_) @name)) @item
