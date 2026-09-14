shader_type canvas_item;

uniform vec4 tint : source_color;

vec4 tint_color(vec4 color) {
	return color * tint;
}

void fragment() {
	COLOR = tint_color(COLOR);
}
