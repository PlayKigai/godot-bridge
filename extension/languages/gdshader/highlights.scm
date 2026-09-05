(comment) @comment

[
  "shader_type"
  "render_mode"
  "group_uniforms"
  "global"
  "instance"
  "const"
  "varying"
  "uniform"
  "struct"
  "for"
  "while"
  "if"
  "else"
  "continue"
  "break"
  "switch"
  "case"
  "default"
  "return"
  "#"
  "include"
] @keyword

(builtin_type) @type.builtin
(ident_type) @type
(precision_qualifier) @keyword
(interpolation_qualifier) @keyword
(param_qualifier) @keyword
(string) @string
(integer) @number
(float) @number
(boolean) @boolean
(builtin_variable) @constant
(ident) @variable

(function_declaration name: (_) @function)
(call_expr function: (_) @function)
(member_expr member: (_) @property)
(uniform_declaration specifier: (var_specifier name: (_) @property))
(struct_member name: (_) @property)
(var_specifier name: (_) @variable)
(parameter name: (_) @variable)
(const_declaration specifier: (var_specifier name: (_) @constant))
(const_var_declaration specifier: (var_specifier name: (_) @constant))

[
  "!"
  "!="
  "%"
  "&"
  "&&"
  "*"
  "+"
  "++"
  "+="
  "-"
  "--"
  "-="
  "/"
  "<"
  "<<"
  "<="
  "="
  "=="
  ">"
  ">="
  ">>"
  "^"
  "|"
  "||"
  "~"
  "?"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
