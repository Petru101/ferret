extends Node
# Enemies run one script and keep their health like the player does (in a LiveStats): a value
# of one of them (the Boss) can only be told apart by its place in the scene tree. No script
# variable holds them.

const LiveStats = preload("res://entities/live_stats.gd")

var current_stats := LiveStats.new()
