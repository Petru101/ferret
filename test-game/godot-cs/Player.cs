using System.Collections.Generic;
using Godot;

namespace Standin;

/// What the player carries: a plain C# object in a List.
public class Item
{
    public int Id;
    public int Count;
}

/// A struct of the game's own inside the player's object.
public struct Stats
{
    public int Strength;
    public float Luck;
}

/// The player: a node. Numbers of every kind Ferret lists, a Godot struct (Vector2), the game's
/// own struct, an array, a List of objects and a Dictionary.
public partial class Player : Node2D
{
    public int Gold = 1000;
    public float Energy = 1.5f;
    public double Shield = 250.25;
    public long Score = 5_000_000_000;
    public bool Shielded = true;
    public Vector2 Speed = new Vector2(320, 240);
    public Stats Stats = new Stats { Strength = 12, Luck = 0.25f };
    public int[] Ammo = { 30, 60, 90 };
    public List<Item> Items = new List<Item> { new Item { Id = 7, Count = 41 }, new Item { Id = 9, Count = 77 } };
    public Dictionary<string, int> Resources = new Dictionary<string, int> { ["wood"] = 12, ["iron ore"] = 40 };
}
