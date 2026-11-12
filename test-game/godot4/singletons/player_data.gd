class_name PlayerData
extends RefCounted

var hp := 25
var gold := 120
var wood := 12
var energy := 50.0
# Stats under whole-number keys (the hashes of their names), as Brotato keeps them.
var effects := {"stat_armor".hash(): 3, "stat_luck".hash(): 7}


func copy() -> PlayerData:
	var p := PlayerData.new()
	p.hp = hp
	p.gold = gold
	p.wood = wood
	p.energy = energy
	p.effects = effects.duplicate()
	return p
