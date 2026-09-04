(comment) @comment

[
  "extends"
  "class_name"
  "class"
  "enum"
  "signal"
  "func"
  "const"
  "var"
  "onready"
  "export"
  "remote"
  "master"
  "puppet"
  "remotesync"
  "mastersync"
  "puppetsync"
  "if"
  "elif"
  "else"
  "for"
  "while"
  "match"
  "when"
  "return"
  "pass"
  "break"
  "continue"
  "await"
  "set"
  "get"
  "setget"
] @keyword

(static_keyword) @keyword
(remote_keyword) @keyword
(breakpoint_statement) @keyword

[(true) (false)] @boolean
(null) @constant

(string) @string
(string_name) @string
(node_path) @string
(escape_sequence) @string.escape
(integer) @number
(float) @number

(identifier) @variable
(function_definition name: (name) @function)
(constructor_definition "_init" @function)
(call (identifier) @function)
(base_call (identifier) @function.method)
(attribute_call (identifier) @function.method)
(attribute_subscript (identifier) @property)
(attribute (identifier) @property)
(type) @type
(const_statement name: (name) @constant)
(enum_definition name: (name) @type)
(class_definition name: (name) @type)
(class_name_statement name: (name) @type)
(variable_statement name: (name) @variable)
(onready_variable_statement name: (name) @variable)
(export_variable_statement name: (name) @property)

[
  "and"
  "as"
  "in"
  "is"
  "not"
  "or"
  "!="
  "%"
  "&"
  "&&"
  "*"
  "**"
  "+"
  "-"
  "/"
  "<"
  "<<"
  "<="
  "=="
  ">"
  ">="
  ">>"
  "^"
  "|"
  "||"
  "~"
  "="
  "+="
  "-="
  "*="
  "/="
  "**="
  "%="
  ">>="
  "<<="
  "&="
  "^="
  "|="
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
