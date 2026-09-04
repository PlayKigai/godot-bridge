(function_definition
  body: (body) @function.inside) @function.around

(constructor_definition
  body: (body) @function.inside) @function.around

(class_definition
  body: (class_body) @class.inside) @class.around
