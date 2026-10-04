extends Node

var wave := 1
var players_data: Array = [PlayerData.new()]
var snapshots: Array = []
var inventory: Array = [
	{"id": 1, "amount": 9, "data": null, "index": 0},
	{"id": 0, "amount": 33, "data": null, "index": 1},
]
var stats := {"metal_collected": 33, "shots_fired": 0}
# StringName keys.
var tools: Array = [
	{&"kind": 3, &"charges": 5, &"slot": 0},
	{&"kind": 4, &"charges": 2, &"slot": 1},
]


func player() -> PlayerData:
	return players_data[0]


func stack(id: int) -> Dictionary:
	for s in inventory:
		if s["id"] == id:
			return s
	return {}


func next_wave() -> void:
	snapshots.append(player().copy())
	wave += 1
