(class_definition
  "class" @context
  name: (name) @name) @item

(function_definition
  "func" @context
  name: (name) @name) @item

(constructor_definition
  "func" @context
  "_init" @name) @item

(signal_statement
  "signal" @context
  name: (name) @name) @item

(const_statement
  "const" @context
  name: (name) @name) @item

(enum_definition
  "enum" @context
  name: (name) @name) @item

(enum_definition
  body: (enumerator_list
    (enumerator
      left: (identifier) @name) @item))

(variable_statement
  "var" @context
  name: (name) @name) @item

(onready_variable_statement
  "onready" @context
  name: (name) @name) @item

(export_variable_statement
  "export" @context
  name: (name) @name) @item
