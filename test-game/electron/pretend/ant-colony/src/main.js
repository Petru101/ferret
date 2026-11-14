// State kept the way entity-component games (bitECS and friends) keep it: in typed arrays,
// one array per field, an entity being an index. Ferret's Browse tab doesn't read typed arrays
// (web.md), so this is the case that shows it. What it gives Ferret:
// - colony[FOOD], colony[TUNNELS]: Int32Array slots (raw 4-byte ints in the array's buffer);
// - queen[HEALTH]: a Float64Array slot (a raw double, 100.0 down by 7.5 a poke);
// - ants: Float32Array energy per ant (drains every frame, fractions);
// - weather: comes from a Web Worker, whose own state (temperature, storms) lives in a second
//   JavaScript heap in the same renderer process.
import { harness } from "../../harness.js";

const MAX = 64;
const FOOD = 0, TUNNELS = 1, ANTS = 2;
const colony = new Int32Array(4);
const HEALTH = 0, EGGS = 1;
const queen = new Float64Array(2);
const energy = new Float32Array(MAX);
const posX = new Float32Array(MAX);
const posY = new Float32Array(MAX);

colony[FOOD] = 120;
colony[TUNNELS] = 3;
queen[HEALTH] = 100;
queen[EGGS] = 4;
let message = "The colony wakes up.";
let weather = { temperature: 21, storms: 0, raining: false };

function hatch() {
  if (colony[ANTS] >= MAX) return;
  const i = colony[ANTS]++;
  energy[i] = 100;
  posX[i] = 430 + (Math.random() - 0.5) * 200;
  posY[i] = 300 + (Math.random() - 0.5) * 80;
}
for (let i = 0; i < 5; i++) hatch();

const worker = new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
worker.onmessage = (e) => (weather = e.data);

const ctx = document.getElementById("c").getContext("2d");
let last = performance.now();
function frame(now) {
  const dt = (now - last) / 1000;
  last = now;
  for (let i = 0; i < colony[ANTS]; i++) {
    energy[i] = Math.max(0, energy[i] - dt * (weather.raining ? 2 : 0.8));
    if (energy[i] < 30 && colony[FOOD] > 0) (colony[FOOD]--, (energy[i] += 50));
    posX[i] += (Math.random() - 0.5) * 3;
    posY[i] += (Math.random() - 0.5) * 3;
  }
  ctx.fillStyle = weather.raining ? "#1f2a33" : "#2a2116";
  ctx.fillRect(0, 0, 860, 440);
  ctx.fillStyle = "#f0e2c8";
  ctx.font = "20px monospace";
  ctx.fillText(`Food ${colony[FOOD]}  Ants ${colony[ANTS]}  Tunnels ${colony[TUNNELS]}  Queen ${queen[HEALTH].toFixed(1)}%  Eggs ${queen[EGGS]}`, 20, 34);
  ctx.font = "16px monospace";
  ctx.fillText(`Weather ${weather.temperature.toFixed(1)}°C${weather.raining ? ", raining" : ""}  Storms ${weather.storms}`, 20, 62);
  ctx.fillText(message, 20, 88);
  for (let i = 0; i < colony[ANTS]; i++) {
    ctx.fillStyle = `hsl(30, 60%, ${20 + energy[i] / 3}%)`;
    ctx.fillRect(posX[i], posY[i], 6, 4);
  }
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);

const on = (id, f) => document.getElementById(id).addEventListener("click", f);
on("feed", () => ((colony[FOOD] += 50), (message = "Crumbs!")));
on("hatch", () => {
  if (colony[FOOD] < 20) return (message = "Not enough food to hatch.");
  if (queen[EGGS] < 1) return (message = "No eggs. Dig a tunnel.");
  colony[FOOD] -= 20;
  queen[EGGS]--;
  hatch();
  message = "An ant hatched.";
});
on("hurt-queen", () => ((queen[HEALTH] = Math.max(0, queen[HEALTH] - 7.5)), (message = "The queen is upset.")));
on("dig", () => ((colony[TUNNELS]++, (queen[EGGS] += 2)), (message = "A new tunnel, two new eggs.")));
on("storm", () => worker.postMessage("storm"));

harness(() => ({
  food: colony[FOOD],
  tunnels: colony[TUNNELS],
  ants: colony[ANTS],
  queenHealth: queen[HEALTH],
  eggs: queen[EGGS],
  energy0: Math.round(energy[0] * 10) / 10,
  weather,
}));
