// Electron stand-ins for web games: `electron . <variant> <cmd file> <log file>`, variant an
// .html file (a real web game, wrapped as an Electron release would be) or classic (an inline script: top-level let/const and var), module (ES modules, state in module
// scope), bundle (the module variant bundled and minified like a Vite build: `npm run build`)
// or phaser (a Phaser game bundled the same way). Commands (one per line, the file is removed
// once read) reach the page by postMessage, so nothing on `window` leads to the game's state:
// hurt N, gold N (adds), kill, revive, wave, churn (garbage, then a collection), quit.
// Each command's resulting state is a line in the log file.
const { app, BrowserWindow } = require("electron");
const fs = require("fs");
const path = require("path");

const [variant, cmd, log] = process.argv.slice(-3);
app.commandLine.appendSwitch("js-flags", "--expose-gc");

app.whenReady().then(() => {
  const win = new BrowserWindow({ width: 800, height: 600, title: `Stand-in (${variant})` });
  // A path to an .html file: that game, wrapped the way an Electron release would be.
  win.loadFile(variant.endsWith(".html") ? variant : path.join(__dirname, variant, "index.html"));
  win.webContents.on("console-message", (e) => {
    const text = e.message ?? "";
    if (text.startsWith("state ")) fs.appendFileSync(log, text.slice(6) + "\n");
    else console.error("page: " + text);
  });
  setInterval(() => {
    if (win.isDestroyed() || !fs.existsSync(cmd)) return;
    const lines = fs.readFileSync(cmd, "utf8").split("\n").filter((l) => l.trim());
    fs.unlinkSync(cmd);
    for (const l of lines) {
      if (l.startsWith("quit")) app.exit(0);
      win.webContents.executeJavaScript(`window.postMessage(${JSON.stringify(l)}, "*")`);
    }
  }, 100);
});
