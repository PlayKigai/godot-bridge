extends Node2D


func _ready() -> void:
	$Label.text = greet("world")


func greet(name: String) -> String:
	return "Hello, %s!" % name
