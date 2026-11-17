(identifier) @variable
(name) @variable
(comment) @comment
(string) @string
(integer) @number
(float) @number
(type) @type
(function_definition name: (name) @function)
(class_definition name: (name) @type)
(call (identifier) @function)
["func" "class" "extends" "if" "elif" "else" "for" "while" "return"
 "var" "const" "signal" "await" "as" "is" "pass" "break" "continue"] @keyword
