extends Node
# Like Brotato's player: made again every wave, so a saved value has to be found in the new
# one; health is in the stats object it holds.

const LiveStats = preload("res://entities/live_stats.gd")

var speed := 450
# An on/off flag (a bool Variant): while on, `hurt` does nothing.
var invincible := false
var current_stats := LiveStats.new()
