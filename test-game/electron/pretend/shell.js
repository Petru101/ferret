// The Electron side every pretend game shares: one window on the game's index.html. Run alone
// (`electron pretend/<game>`) it's a game to play by hand. With FERRET_CMD and FERRET_LOG set
// (play.sh --hidden does it), commands are read from the FERRET_CMD file (one per line, the file
// is removed once read) and passed to the page by postMessage, so nothing on `window` leads to
// the game's state:
//   click <button id> [times]   presses one of the game's buttons
//   report                      just writes the state
//   quit
// The page answers each with a "state {...}" console line, appended to FERRET_LOG.
const { app, BrowserWindow } = require("electron");
const fs = require("fs");
const path = require("path");

module.exports = function shell(dir, { width = 900, height = 640 } = {}) {
  const cmd = process.env.FERRET_CMD;
  const log = process.env.FERRET_LOG;
  app.whenReady().then(() => {
    // Hidden runs (Xvfb, no window manager): Chromium takes the window for a background one and
    // all but stops its animation frames, so the games' loops would stand still.
    const webPreferences = { contextIsolation: true, backgroundThrottling: !cmd };
    const win = new BrowserWindow({ width, height, autoHideMenuBar: true, webPreferences });
    win.loadFile(path.join(dir, "index.html"));
    win.webContents.on("console-message", (e) => {
      const text = e.message ?? "";
      if (text.startsWith("state ")) {
        if (log) fs.appendFileSync(log, text.slice(6) + "\n");
      } else console.error("page: " + text);
    });
    if (!cmd) return;
    setInterval(() => {
      if (win.isDestroyed() || !fs.existsSync(cmd)) return;
      const lines = fs.readFileSync(cmd, "utf8").split("\n").filter((l) => l.trim());
      fs.unlinkSync(cmd);
      for (const l of lines) {
        if (l.startsWith("quit")) return app.exit(0);
        win.webContents.executeJavaScript(`window.postMessage(${JSON.stringify(l)}, "*")`);
      }
    }, 100);
  });
  app.on("window-all-closed", () => app.quit());
};
