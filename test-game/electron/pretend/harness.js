// The page side of shell.js's commands, for the bundled games: `harness(() => state)`. The
// listener's closure is the only thing holding `getState` (and through it the game), the way
// a real game's state hangs off the browser's listeners.
export function harness(getState) {
  const report = () => console.log("state " + JSON.stringify(getState()));
  window.addEventListener("message", (e) => {
    const [c, id, n] = String(e.data).split(" ");
    if (c === "click") {
      const b = document.getElementById(id);
      if (!b) console.log("no button " + id);
      else for (let i = 0; i < (Number(n) || 1); i++) b.click();
    }
    // React and other frameworks update after the click: report a moment later.
    if (c === "click" || c === "report") setTimeout(report, 50);
  });
  setTimeout(report, 500);
}
