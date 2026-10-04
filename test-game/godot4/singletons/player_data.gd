class_name PlayerData
extends RefCounted

var hp := 25
var gold := 120
var wood := 12
var energy := 50.0


func copy() -> PlayerData:
	var p := PlayerData.new()
	p.hp = hp
	p.gold = gold
	p.wood = wood
	p.energy = energy
	return p
