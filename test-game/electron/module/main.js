// ES modules, the way a Vite project starts: the game object lives in module scope, reachable
// from nothing on `window` (only the browser's own listeners hold the code).
import { Ship } from "./player.js";

class Game {
  constructor(canvas) {
    this.ctx = canvas.getContext("2d");
    this.ship = new Ship();
    this.wave = 1;
    this.score = 0;
    this.alive = true;
    this.asteroids = [{ size: 3, hp: 40 }, { size: 1, hp: 10 }];
  }

  draw() {
    const c = this.ctx;
    c.fillStyle = "#000";
    c.fillRect(0, 0, 800, 600);
    c.fillStyle = "#fff";
    c.font = "20px monospace";
    c.fillText(`Hull ${this.ship.hull}  Credits ${this.ship.credits}  Wave ${this.wave}  Score ${this.score}`, 20, 30);
  }
}

const game = new Game(document.getElementById("c"));
let frames = 0;

function loop() {
  frames++;
  game.draw();
  requestAnimationFrame(loop);
}
requestAnimationFrame(loop);

function report() {
  const s = game.ship;
  console.log("state " + JSON.stringify({ hp: s.hull, gold: s.credits, alive: game.alive, wave: game.wave, score: game.score, iron: s.cargo.get("iron"), shields: s.shieldsUp }));
}

window.addEventListener("message", (e) => {
  const [c, n] = String(e.data).split(" ");
  const v = Number(n) || 0;
  if (c === "hurt") game.ship.damage(v);
  if (c === "gold") game.ship.credits += v;
  if (c === "kill") game.alive = false;
  if (c === "revive") game.alive = true;
  if (c === "wave") { game.wave++; game.score += 100; }
  if (c === "churn") { for (let i = 0; i < 200000; i++) new Array(16).fill(i); if (window.gc) window.gc(); }
  report();
});
report();
