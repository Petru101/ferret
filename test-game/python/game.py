#!/usr/bin/env python3
# A Python stand-in for Ferret's Python reading (helper/src/python.rs): a game's globals, a player
# object with attributes, a list of items. Every half second the player regenerates and the game
# writes what it sees to `game.log` (one line: score lives hp gold stack counts, then the flags poisoned= and paused=), so a test can
# check the game reads what Ferret wrote. Runs until killed.
import os
import sys
import time


class Item:
    def __init__(self, name, count):
        self.name = name
        self.count = count


class Player:
    def __init__(self):
        self.hp = 75.5
        self.max_hp = 100.0
        self.gold = 1234
        self.level = 3
        # An on/off flag: while True the player doesn't regenerate.
        self.poisoned = False
        self.inventory = [Item("arrow", 40), Item("potion", 3)]


score = 4321
lives = 3
paused = False
player = Player()
log = os.path.join(os.path.dirname(os.path.abspath(__file__)), "game.log")

while True:
    # A new float object each time (Python's numbers are immutable): the hp the game sees is
    # whatever its attribute points to now.
    if not player.poisoned:
        player.hp = min(player.max_hp, player.hp + 0.5)
    with open(log, "w") as f:
        f.write(f"{score} {lives} {player.hp} {player.gold} {' '.join(str(i.count) for i in player.inventory)} poisoned={player.poisoned} paused={paused}\n")
    sys.stdout.flush()
    time.sleep(0.5)
