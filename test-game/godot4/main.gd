extends Control

# Commands come from a file (one line, removed once read), as in target.c:
#   earn/spend N (gold), wood N, hp N, energy X (float), iron N (inventory dictionary,
#   id 0, also counted in stats), lumen N (id 1), wave (keeps a snapshot of the player),
#   show, quit N.
# Run: <binary> -- <cmd file> <log file>

var cmd_path := ""
var log_path := ""
var since := 0.0

@onready var label: Label = $Label


func _ready() -> void:
	var args := OS.get_cmdline_user_args()
	if args.size() >= 2:
		cmd_path = args[0]
		log_path = args[1]
	report()


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
		"wave":
			RunData.next_wave()
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
	f.store_line("gold=%d wood=%d hp=%d energy=%.2f iron=%d lumen=%d metal_collected=%d wave=%d snapshots=%d" % [
		p.gold, p.wood, p.hp, p.energy, RunData.stack(0)["amount"], RunData.stack(1)["amount"],
		RunData.stats["metal_collected"], RunData.wave, RunData.snapshots.size()])
	f.close()
