// An idle clicker the way many small Electron games are written: no canvas and no engine,
// just buttons and text, state in module scope (minified away by the bundle). What it gives
// Ferret to find:
// - moss: a double with fractions that changes every frame (per-second income);
// - bank: a whole number past 2^31 (3 billion coins: never a small integer);
// - gems: a small whole number that only changes on a click;
// - helpers: an array of objects of one shape, each with its own count and growing cost;
// - multiplier: a fraction (1.25 after a compost).
import { harness } from "../../harness.js";

const state = {
  moss: 0,
  totalMoss: 0,
  perClick: 1,
  gems: 0,
  bank: 3_000_000_000,
  multiplier: 1,
  composts: 0,
};

const helpers = [
  { id: "spores", name: "Spore cloud", base: 15, cost: 15, owned: 0, rate: 0.1 },
  { id: "snail", name: "Garden snail", base: 100, cost: 100, owned: 0, rate: 1 },
  { id: "stone", name: "Mossy stone", base: 1100, cost: 1100, owned: 0, rate: 8 },
  { id: "forest", name: "Old forest", base: 12000, cost: 12000, owned: 0, rate: 47 },
];

const $ = (id) => document.getElementById(id);
const fmt = (n) => (n < 1e6 ? (Math.floor(n * 10) / 10).toLocaleString("en-US") : n.toExponential(3));
const say = (t) => ($("log").textContent = t);

function perSecond() {
  return helpers.reduce((s, h) => s + h.owned * h.rate, 0) * state.multiplier;
}

function gain(n) {
  state.moss += n;
  state.totalMoss += n;
}

const buttons = new Map();
for (const h of helpers) {
  const b = document.createElement("button");
  b.id = "buy-" + h.id;
  b.addEventListener("click", () => {
    if (state.moss < h.cost) return say(`Not enough moss for a ${h.name.toLowerCase()}.`);
    state.moss -= h.cost;
    h.owned++;
    h.cost = Math.ceil(h.cost * 1.15);
    say(`Bought a ${h.name.toLowerCase()}.`);
  });
  $("shop").append(b);
  buttons.set(h, b);
}

$("harvest").addEventListener("click", () => gain(state.perClick * state.multiplier));
$("rain").addEventListener("click", () => {
  state.gems++;
  gain(50);
  say("It rained. A gem washed up.");
});
$("sell").addEventListener("click", () => {
  if (state.moss < 1000) return say("You need 1000 moss to sell.");
  state.moss -= 1000;
  state.bank += 250;
  say("Sold 1000 moss for 250 coins.");
});
$("prestige").addEventListener("click", () => {
  if (state.totalMoss < 10000) return say("Grow 10,000 moss in total before composting.");
  state.composts++;
  state.multiplier = 1 + state.composts * 0.25;
  state.moss = 0;
  state.perClick = 1 + state.composts;
  for (const h of helpers) (h.owned = 0), (h.cost = h.base);
  say(`Composted. Everything grows x${state.multiplier} now.`);
});

let last = performance.now();
function frame(now) {
  gain((perSecond() * (now - last)) / 1000);
  last = now;
  $("moss").textContent = fmt(state.moss);
  $("mps").textContent = fmt(perSecond());
  $("mpc").textContent = fmt(state.perClick * state.multiplier);
  $("gems").textContent = state.gems;
  $("bank").textContent = state.bank.toLocaleString("en-US");
  $("mult").textContent = state.multiplier;
  for (const [h, b] of buttons) {
    b.textContent = `${h.name} (${h.owned}) - ${fmt(h.cost)} moss`;
    b.disabled = state.moss < h.cost;
  }
  requestAnimationFrame(frame);
}
requestAnimationFrame(frame);

harness(() => ({
  moss: Math.floor(state.moss * 100) / 100,
  gems: state.gems,
  bank: state.bank,
  multiplier: state.multiplier,
  perSecond: perSecond(),
  helpers: Object.fromEntries(helpers.map((h) => [h.id, h.owned])),
}));
