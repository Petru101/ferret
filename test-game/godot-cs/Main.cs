using System;
using System.Globalization;
using System.IO;
using Godot;

namespace Standin;

/// Ferret's C# Godot stand-in: `standin-cs.x86_64 [--headless] -- <command file> <log file>`.
/// A command file holds one command; it is read and removed five times a second, and each
/// one is answered with a state line in the log. Commands: gold N, energy X, shield X, score N,
/// shielded 0|1, speedx X, speedy X, str N, luck X, ammo I N, item I N (Items[I].Count),
/// wood N, ore N, coins N, difficulty X (statics), wave N, timer X (the singleton), respawn
/// (a new Player node, the old one freed), grow (20 more items: the List's array moves), gc
/// (a compacting collection: objects move), show, quit N.
public partial class Main : Node2D
{
    private Player _player;
    private string _cmd, _log;
    private double _since;

    public override void _Ready()
    {
        var args = OS.GetCmdlineUserArgs();
        _cmd = args.Length > 0 ? args[0] : "cmd";
        _log = args.Length > 1 ? args[1] : "log";
        Spawn();
        Say();
    }

    private void Spawn()
    {
        _player = new Player { Name = "Player" };
        AddChild(_player);
    }

    public override void _Process(double delta)
    {
        _since += delta;
        if (_since < 0.2) return;
        _since = 0;
        if (!File.Exists(_cmd)) return;
        string line;
        try { line = File.ReadAllText(_cmd).Trim(); File.Delete(_cmd); } catch (IOException) { return; }
        var p = line.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        if (p.Length == 0) return;
        double X(int i) => p.Length > i ? double.Parse(p[i], CultureInfo.InvariantCulture) : 0;
        int N(int i) => (int)X(i);
        switch (p[0])
        {
            case "gold": _player.Gold = N(1); break;
            case "energy": _player.Energy = (float)X(1); break;
            case "shield": _player.Shield = X(1); break;
            case "score": _player.Score = (long)X(1); break;
            case "shielded": _player.Shielded = N(1) != 0; break;
            case "speedx": _player.Speed.X = (float)X(1); break;
            case "speedy": _player.Speed.Y = (float)X(1); break;
            case "str": _player.Stats.Strength = N(1); break;
            case "luck": _player.Stats.Luck = (float)X(1); break;
            case "ammo": _player.Ammo[N(1)] = N(2); break;
            case "item": _player.Items[N(1)].Count = N(2); break;
            case "wood": _player.Resources["wood"] = N(1); break;
            case "ore": _player.Resources["iron ore"] = N(1); break;
            case "coins": GameState.Coins = N(1); break;
            case "difficulty": GameState.Difficulty = (float)X(1); break;
            case "wave": Run.Instance.Wave = N(1); break;
            case "timer": Run.Instance.Timer = X(1); break;
            case "respawn":
                _player.Free();
                Spawn();
                break;
            case "grow":
                for (int i = 0; i < 20; i++) _player.Items.Add(new Item { Id = 100 + i, Count = 1 });
                break;
            case "gc":
                System.Runtime.GCSettings.LargeObjectHeapCompactionMode = System.Runtime.GCLargeObjectHeapCompactionMode.CompactOnce;
                GC.Collect(2, GCCollectionMode.Forced, true, true);
                break;
            case "quit":
                Say();
                GetTree().Quit(N(1));
                return;
        }
        Say();
    }

    private void Say()
    {
        var pl = _player;
        var c = CultureInfo.InvariantCulture;
        string items = "";
        for (int i = 0; i < Math.Min(pl.Items.Count, 3); i++) items += (i > 0 ? "," : "") + pl.Items[i].Count;
        string line = string.Format(c,
            "gold={0} energy={1} shield={2} score={3} shielded={4} speed={5},{6} str={7} luck={8} ammo={9},{10},{11} items={12} ({13}) wood={14} ore={15} coins={16} difficulty={17} wave={18} timer={19}",
            pl.Gold, pl.Energy, pl.Shield, pl.Score, pl.Shielded ? 1 : 0, pl.Speed.X, pl.Speed.Y, pl.Stats.Strength, pl.Stats.Luck,
            pl.Ammo[0], pl.Ammo[1], pl.Ammo[2], items, pl.Items.Count, pl.Resources["wood"], pl.Resources["iron ore"],
            GameState.Coins, GameState.Difficulty, Run.Instance.Wave, Run.Instance.Timer);
        File.AppendAllText(_log, line + "\n");
        var label = GetNodeOrNull<Label>("Label");
        if (label != null) label.Text = line.Replace(' ', '\n');
    }
}
