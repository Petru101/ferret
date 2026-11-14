// A PixiJS 8 game bundled like a Vite build. Everything lives inside `async function main()`
// (a function context, not module scope), the shape a lot of PixiJS starter code has. What it
// gives Ferret:
// - the player: a class extending Pixi's Container, so its own fields (health, ammo, hearts)
//   sit next to dozens of Pixi's (position, scale, children...);
// - score and combo: `let`s in main()'s closure; combo is a fraction (1.0, 1.5, 2.25...);
// - petals: an array of sprites-with-data (each a Graphics with a `hp` field).
import { Application, Container, Graphics, Text } from "pixi.js";
import { harness } from "../../harness.js";

class Player extends Container {
  constructor() {
    super();
    this.health = 5;
    this.maxHealth = 5;
    this.ammo = 12;
    this.clipSize = 12;
    this.speed = 4.5;
    this.invincible = false;
    const body = new Graphics().circle(0, 0, 18).fill(0x7ee0ff);
    this.addChild(body);
  }
}

async function main() {
  const app = new Application();
  await app.init({ width: 860, height: 460, background: 0x0b0b18, antialias: true });
  document.getElementById("stage").append(app.canvas);

  let score = 0;
  let combo = 1;
  let wave = 1;
  let message = "Petals incoming.";

  const player = new Player();
  player.position.set(430, 400);
  app.stage.addChild(player);

  const petals = [];
  function spawnPetal() {
    const g = new Graphics().star(0, 0, 5, 16, 8).fill(0xff9ad5);
    g.hp = 2 + wave;
    g.position.set(60 + Math.random() * 740, -20);
    app.stage.addChild(g);
    petals.push(g);
  }
  for (let i = 0; i < 4; i++) spawnPetal();

  const hud = new Text({ text: "", style: { fill: 0xffffff, fontFamily: "monospace", fontSize: 20 } });
  hud.position.set(16, 12);
  app.stage.addChild(hud);

  app.ticker.add((t) => {
    player.x = 430 + Math.sin(performance.now() / 900) * 300;
    for (const p of petals) {
      p.y += 0.6 * t.deltaTime;
      p.rotation += 0.02 * t.deltaTime;
      if (p.y > 470) p.y = -20;
    }
    hud.text = `Health ${"♥".repeat(player.health)}${"♡".repeat(Math.max(0, player.maxHealth - player.health))}  Ammo ${player.ammo}/${player.clipSize}  Score ${score}  Combo x${combo}  Wave ${wave}\n${message}`;
  });

  const on = (id, f) => document.getElementById(id).addEventListener("click", f);
  on("shoot", () => {
    if (player.ammo === 0) return (message = "Click. Reload!");
    player.ammo--;
    const target = petals[0];
    if (!target) return;
    target.hp--;
    score += Math.round(10 * combo);
    if (target.hp <= 0) {
      app.stage.removeChild(target);
      petals.shift();
      spawnPetal();
      message = "Petal down.";
      if (score > wave * 300) (wave++, (message = `Wave ${wave}.`));
    }
  });
  on("reload", () => ((player.ammo = player.clipSize), (message = "Reloaded.")));
  on("hurt", () => {
    if (player.invincible) return;
    player.health = Math.max(0, player.health - 1);
    combo = 1;
    message = player.health ? "Ouch." : "You wilted. (Buy a heart.)";
  });
  on("bloom", () => ((combo = Math.round(combo * 1.5 * 100) / 100), (message = `Combo x${combo}!`)));
  on("shop", () => {
    if (score < 200) return (message = "200 points for a heart.");
    score -= 200;
    player.maxHealth++;
    player.health = player.maxHealth;
    message = "A new heart.";
  });

  harness(() => ({ health: player.health, maxHealth: player.maxHealth, ammo: player.ammo, score, combo, wave, petals: petals.length }));
}

main();
