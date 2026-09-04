(comment) @comment
(string) @string
(string_name) @string
(path) @string
(integer) @number
(float) @number
(true) @boolean
(false) @boolean
(null) @constant

(property (path) @property)
(attribute (identifier) @property)
(section (identifier) @type)
(section (identifier) @keyword)
(constructor (identifier) @function)
(property (identifier) @variable)
(attribute (identifier) @variable)

"=" @operator
["(" ")" "[" "]"] @punctuation.bracket
