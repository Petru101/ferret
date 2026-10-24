export class Ship {
  constructor() {
    this.hull = 100;
    this.maxHull = 100;
    this.fuel = 62.5;
    this.credits = 1500;
    this.shieldsUp = true;
    this.cargo = new Map([["iron", 12], ["crystal", 2]]);
    this.upgrades = { engine: 1, laser: 2 };
  }

  damage(n) {
    if (!this.shieldsUp) this.hull = Math.max(0, this.hull - n);
  }
}
