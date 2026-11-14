// The colony's weather, simulated off the main thread: this state lives in the worker's own
// JavaScript heap (an isolate of its own), not the page's.
const sky = { temperature: 21, storms: 0, raining: false, rainLeft: 0 };

setInterval(() => {
  sky.temperature += (Math.random() - 0.5) * 0.4;
  if (sky.raining && --sky.rainLeft <= 0) sky.raining = false;
  postMessage({ temperature: sky.temperature, storms: sky.storms, raining: sky.raining });
}, 250);

onmessage = (e) => {
  if (e.data === "storm") {
    sky.storms++;
    sky.raining = true;
    sky.rainLeft = 40;
    sky.temperature -= 3;
  }
};
