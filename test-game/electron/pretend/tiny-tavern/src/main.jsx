// A React 19 game, the way many AI-made "management" games are built: no canvas, every number
// in React state. React keeps a component's hooks as a linked list on its fiber
// (fiber.memoizedState -> {memoizedState, queue, next}), so the numbers have no names in memory;
// only the order of the hooks tells them apart. What it gives Ferret:
// - gold, ale (whole numbers), reputation (a fraction, 3.0 to 5.0): useState hooks;
// - staff: a useReducer object {cooks, bards, wages};
// - guests: an array in state, replaced (not changed) on every update, as React wants.
import { useEffect, useReducer, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { harness } from "../../harness.js";

function staffReducer(s, a) {
  switch (a) {
    case "cook": return { ...s, cooks: s.cooks + 1, wages: s.wages + 4 };
    case "bard": return { ...s, bards: s.bards + 1, wages: s.wages + 6 };
    default: return s;
  }
}

const NAMES = ["Mira", "Tobb", "Old Gant", "Fen", "Isolde", "Pip", "Hroth", "Wren"];

function Tavern() {
  const [gold, setGold] = useState(75);
  const [ale, setAle] = useState(20);
  const [reputation, setReputation] = useState(3.0);
  const [day, setDay] = useState(1);
  const [guests, setGuests] = useState(["Mira", "Tobb"]);
  const [staff, dispatch] = useReducer(staffReducer, { cooks: 1, bards: 0, wages: 4 });
  const [message, setMessage] = useState("The doors open.");

  // harness() reads the latest values through a ref, so the listener never holds stale state.
  const now = useRef(null);
  now.current = { gold, ale, reputation, day, guests: guests.length, staff };
  useEffect(() => harness(() => now.current), []);

  const serve = () => {
    if (ale < 1) return setMessage("Out of ale!");
    if (guests.length === 0) return setMessage("Nobody to serve.");
    const tip = Math.round(3 + reputation * 2 + staff.bards * 2);
    setAle((a) => a - 1);
    setGold((g) => g + tip);
    setReputation((r) => Math.min(5, Math.round((r + 0.05) * 100) / 100));
    setMessage(`${guests[0]} tips ${tip} gold.`);
  };
  const brew = () => {
    if (gold < 10) return setMessage("A barrel costs 10 gold.");
    setGold((g) => g - 10);
    setAle((a) => a + 12);
    setMessage("A fresh barrel.");
  };
  const hire = (who) => {
    const cost = who === "cook" ? 40 : 60;
    if (gold < cost) return setMessage(`A ${who} wants ${cost} gold up front.`);
    setGold((g) => g - cost);
    dispatch(who);
    setMessage(`Hired a ${who}.`);
  };
  const nextDay = () => {
    setDay((d) => d + 1);
    setGold((g) => g - staff.wages);
    setGuests(NAMES.slice(0, Math.min(NAMES.length, 1 + Math.floor(reputation) + staff.bards)));
    setMessage(`Day ${day + 1}. Paid ${staff.wages} gold in wages.`);
  };
  const brawl = () => {
    setReputation((r) => Math.max(0, Math.round((r - 0.5) * 100) / 100));
    setGuests((g) => g.slice(1));
    setMessage("A brawl! Someone storms out.");
  };

  return (
    <main>
      <h1>Tiny Tavern</h1>
      <div className="msg">{message}</div>
      <div className="row">
        <div className="stat">Day {day}</div>
        <div className="stat">Gold {gold}</div>
        <div className="stat">Ale {ale} mugs</div>
        <div className="stat">Reputation {reputation.toFixed(2)} ★</div>
        <div className="stat">Wages {staff.wages}/day</div>
      </div>
      <div className="row">
        <button id="serve" onClick={serve}>Serve a mug</button>
        <button id="brew" onClick={brew}>Brew a barrel (10 gold)</button>
        <button id="hire-cook" onClick={() => hire("cook")}>Hire a cook (40)</button>
        <button id="hire-bard" onClick={() => hire("bard")}>Hire a bard (60)</button>
        <button id="day" onClick={nextDay}>Close for the night</button>
        <button id="brawl" onClick={brawl}>Start a brawl</button>
      </div>
      <h2>Guests</h2>
      <ul>{guests.map((g) => <li key={g}>{g}</li>)}</ul>
      <h2>Staff</h2>
      <p>{staff.cooks} cook(s), {staff.bards} bard(s)</p>
    </main>
  );
}

createRoot(document.getElementById("root")).render(<Tavern />);
