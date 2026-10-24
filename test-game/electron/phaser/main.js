// A Phaser 3 game bundled like a Vite build: the scene keeps the state in its own fields.
import Phaser from "phaser";

class MainScene extends Phaser.Scene {
  constructor() {
    super("main");
  }

  create() {
    this.health = 100;
    this.coins = 250;
    this.level = 1;
    this.score = 0;
    this.invincible = false;
    this.alive = true;
    this.player = this.add.rectangle(400, 300, 24, 24, 0x44aaff);
    this.player.setData("lives", 3);
    this.hud = this.add.text(16, 16, "", { fontSize: "20px", color: "#fff" });
    window.addEventListener("message", (e) => this.command(String(e.data)));
    this.report();
  }

  command(line) {
    const [c, n] = line.split(" ");
    const v = Number(n) || 0;
    if (c === "hurt" && !this.invincible) this.health = Math.max(0, this.health - v);
    if (c === "gold") this.coins += v;
    if (c === "kill") this.alive = false;
    if (c === "revive") this.alive = true;
    if (c === "wave") { this.level++; this.score += 100; }
    if (c === "churn") { for (let i = 0; i < 200000; i++) new Array(16).fill(i); if (window.gc) window.gc(); }
    this.report();
  }

  report() {
    console.log("state " + JSON.stringify({ hp: this.health, gold: this.coins, alive: this.alive, wave: this.level, score: this.score, lives: this.player.getData("lives"), invincible: this.invincible }));
  }

  update() {
    this.hud.setText(`Health ${this.health}  Coins ${this.coins}  Level ${this.level}`);
  }
}

new Phaser.Game({ type: Phaser.AUTO, width: 800, height: 600, backgroundColor: "#222", scene: MainScene });
