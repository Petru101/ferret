extends Control

# Commands come from a file (one line, removed once read), as in target.c:
#   earn/spend N (gold), wood N, hp N, energy X (float), iron N (inventory dictionary,
#   id 0, also counted in stats), lumen N (id 1), charges N (tools dictionary with
#   StringName keys, kind 3), wave (keeps a snapshot of the player), hurt N (health of the
#   player node, entities/player.gd), despawn / spawn / respawn (frees the player node and
#   makes a new one, as Brotato does every wave), hit N (health of the enemy named Boss; two
#   unnamed enemies run the same script), show, quit N.
# Run: <binary> -- <cmd file> <log file>

var cmd_path := ""
var log_path := ""
var since := 0.0
var player: Node = null

@onready var label: Label = $Label


func _ready() -> void:
	var args := OS.get_cmdline_user_args()
	if args.size() >= 2:
		cmd_path = args[0]
		log_path = args[1]
	spawn()
	for i in 3:
		var enemy: Node = preload("res://entities/enemy.gd").new()
		if i == 1:
			enemy.name = "Boss"
		add_child(enemy)
	report()


func spawn() -> void:
	player = preload("res://entities/player.gd").new()
	add_child(player)


func despawn() -> void:
	if player != null:
		player.queue_free()
		player = null


func _process(delta: float) -> void:
	since += delta
	if since < 0.1 or cmd_path == "" or not FileAccess.file_exists(cmd_path):
		return
	since = 0.0
	var f := FileAccess.open(cmd_path, FileAccess.READ)
	if f == null:
		return
	var line := f.get_line().strip_edges()
	f.close()
	DirAccess.remove_absolute(cmd_path)
	run(line)
	report()


func run(line: String) -> void:
	var words := line.split(" ", false)
	if words.is_empty():
		return
	var arg := words[1] if words.size() > 1 else "0"
	var p := RunData.player()
	match words[0]:
		"earn":
			p.gold += int(arg)
		"spend":
			p.gold -= int(arg)
		"wood":
			p.wood += int(arg)
		"hp":
			p.hp += int(arg)
		"energy":
			p.energy += float(arg)
		"iron":
			RunData.stack(0)["amount"] += int(arg)
			if int(arg) > 0:
				RunData.stats["metal_collected"] += int(arg)
		"lumen":
			RunData.stack(1)["amount"] += int(arg)
		"charges":
			RunData.tools[0][&"charges"] += int(arg)
		"wave":
			RunData.next_wave()
		"hurt":
			if player != null:
				player.current_stats.health -= int(arg)
		"hit":
			$Boss.current_stats.health -= int(arg)
		"despawn":
			despawn()
		"spawn":
			if player == null:
				spawn()
		"respawn":
			despawn()
			spawn()
		"quit":
			get_tree().quit(int(arg))


func report() -> void:
	var p := RunData.player()
	label.text = "Gold %d   Wood %d   Iron %d   Lumen %d\nEnergy %.1f   HP %d   Wave %d" % [
		p.gold, p.wood, RunData.stack(0)["amount"], RunData.stack(1)["amount"],
		p.energy, p.hp, RunData.wave]
	if log_path == "":
		return
	var f := FileAccess.open(log_path, FileAccess.READ_WRITE)
	if f == null:
		f = FileAccess.open(log_path, FileAccess.WRITE)
	f.seek_end()
	var health: int = player.current_stats.health if player != null else -1
	f.store_line("gold=%d wood=%d hp=%d energy=%.2f iron=%d lumen=%d charges=%d metal_collected=%d wave=%d snapshots=%d health=%d boss=%d" % [
		p.gold, p.wood, p.hp, p.energy, RunData.stack(0)["amount"], RunData.stack(1)["amount"],
		RunData.tools[0][&"charges"], RunData.stats["metal_collected"], RunData.wave, RunData.snapshots.size(), health,
		$Boss.current_stats.health])
	f.close()
