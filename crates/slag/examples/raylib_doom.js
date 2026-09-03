// DOOM-flavored textured raycaster in JavaScript on raylib, using real DOOM
// sprite frames and SFX (crates/slag/examples/DOOM). Walls are procedural
// textures; actors are the original sprites, loaded with rl.loadTexture and
// drawn as distance-scaled billboards.
//
// Controls: WASD move, mouse look, click fire, Shift run, R restart, ESC quit.

// ---- colors / helpers -----------------------------------------------------

function rgba(r, g, b, a) {
    return ((r * 256 + g) * 256 + b) * 256 + (a & 255);
}
function hash2(x, y) {
    const n = Math.sin(x * 127.1 + y * 311.7) * 43758.5453;
    return n - Math.floor(n);
}
const SCREEN_W = 640, SCREEN_H = 480;
const TEX_W = 64;

// asset roots: cargo examples run from crates/slag, the CLI from the repo root
const ROOTS = ["examples/DOOM/", "crates/slag/examples/DOOM/"];

function loadTexture(rel) {
    for (const root of ROOTS) {
        const id = rl.loadTexture(root + rel);
        if (id >= 0) return id;
    }
    return -1;
}
function loadSound(rel) {
    for (const root of ROOTS) {
        const id = rl.loadSound(root + rel);
        if (id >= 0) return id;
    }
    return -1;
}

// ---- procedural wall textures ---------------------------------------------

function buildTexture(w, h, fn) {
    const out = new Array(w * h);
    const HEX = "0123456789abcdef";
    function hex8(v) {
        let s = "";
        for (let i = 28; i >= 0; i -= 4) s += HEX.charAt((v >>> i) & 15);
        return s;
    }
    for (let y = 0; y < h; y++) {
        for (let x = 0; x < w; x++) out[y * w + x] = hex8(fn(x, y));
    }
    return out.join("");
}
function brick(x, y, base, dark) {
    if (y % 8 === 0 || y % 8 === 1) return dark;
    const row = Math.floor(y / 8);
    const col = Math.floor((x + (row % 2) * 4) / 8);
    const r = (base >>> 24) & 255, g = (base >>> 16) & 255, b = (base >>> 8) & 255;
    const f = ((col + row) % 2 === 0 ? 1 : 0.78) * (0.84 + hash2(x, y) * 0.3);
    return rgba(Math.min(255, r * f) | 0, Math.min(255, g * f) | 0, Math.min(255, b * f) | 0, 255);
}
function tech(x, y, base, dark) {
    if (x % 16 === 0 || y % 16 === 0) return dark;
    const rivet = x % 16 === 8 && y % 16 === 8;
    const r = (base >>> 24) & 255, g = (base >>> 16) & 255, b = (base >>> 8) & 255;
    const f = rivet ? 1.45 : 0.72 + hash2(x, y) * 0.4;
    return rgba(Math.min(255, r * f) | 0, Math.min(255, g * f) | 0, Math.min(255, b * f) | 0, 255);
}

const wallTex = [];
function buildWallTextures() {
    wallTex[1] = rl.makeTexture(TEX_W, TEX_W, buildTexture(TEX_W, TEX_W,
        (x, y) => brick(x, y, rgba(148, 60, 44, 255), rgba(54, 26, 22, 255))));
    wallTex[2] = rl.makeTexture(TEX_W, TEX_W, buildTexture(TEX_W, TEX_W,
        (x, y) => tech(x, y, rgba(80, 104, 128, 255), rgba(24, 32, 42, 255))));
    wallTex[3] = rl.makeTexture(TEX_W, TEX_W, buildTexture(TEX_W, TEX_W,
        (x, y) => brick(x, y, rgba(120, 120, 128, 255), rgba(46, 48, 54, 255))));
}

// ---- world ----------------------------------------------------------------

const MW = 32, MH = 20;
const cells = new Uint8Array(MW * MH);
function cellAt(x, y) {
    if (x < 0 || y < 0 || x >= MW || y >= MH) return 1;
    return cells[y * MW + x];
}
function solid(x, y) {
    return cellAt(x, y) !== 0;
}
function carveRect(x0, y0, x1, y1) {
    for (let y = y0; y <= y1; y++) {
        for (let x = x0; x <= x1; x++) {
            if (x >= 0 && y >= 0 && x < MW && y < MH) cells[y * MW + x] = 0;
        }
    }
}
function generateLevel() {
    for (let y = 0; y < MH; y++) {
        for (let x = 0; x < MW; x++) cells[y * MW + x] = 1 + ((x * 7 + y * 13) % 3);
    }
    carveRect(2, 2, 10, 6);
    carveRect(13, 2, 19, 6);
    carveRect(22, 2, 30, 6);
    carveRect(2, 13, 10, 17);
    carveRect(13, 13, 19, 17);
    carveRect(22, 13, 30, 17);
    carveRect(2, 7, 30, 12);
    for (let y = 7; y <= 12; y++) {
        cells[y * MW + 1] = 0;
        cells[y * MW + 30] = 0;
    }
    const pillars = [[6, 4], [8, 4], [16, 4], [26, 4], [28, 4],
                     [6, 15], [8, 15], [16, 15], [26, 15], [28, 15],
                     [5, 9], [12, 9], [20, 9], [27, 9]];
    for (const p of pillars) cells[p[1] * MW + p[0]] = 2;
}

// ---- raycaster ------------------------------------------------------------

function castRay(px, py, rdx, rdy) {
    let mapX = Math.floor(px), mapY = Math.floor(py);
    const deltaX = Math.abs(rdx) < 1e-9 ? 1e30 : Math.abs(1 / rdx);
    const deltaY = Math.abs(rdy) < 1e-9 ? 1e30 : Math.abs(1 / rdy);
    let stepX, stepY, sideDistX, sideDistY;
    if (rdx < 0) { stepX = -1; sideDistX = (px - mapX) * deltaX; }
    else { stepX = 1; sideDistX = (mapX + 1 - px) * deltaX; }
    if (rdy < 0) { stepY = -1; sideDistY = (py - mapY) * deltaY; }
    else { stepY = 1; sideDistY = (mapY + 1 - py) * deltaY; }
    let side = 0;
    for (let guard = 0; guard < 128; guard++) {
        if (sideDistX < sideDistY) { sideDistX += deltaX; mapX += stepX; side = 0; }
        else { sideDistY += deltaY; mapY += stepY; side = 1; }
        if (solid(mapX, mapY)) break;
    }
    const perp = side === 0 ? sideDistX - deltaX : sideDistY - deltaY;
    const t = cellAt(mapX, mapY);
    let wallX = side === 0 ? py + perp * rdy : px + perp * rdx;
    wallX -= Math.floor(wallX);
    let texX = Math.floor(wallX * TEX_W);
    if ((side === 0 && rdx > 0) || (side === 1 && rdy < 0)) texX = TEX_W - texX - 1;
    return { dist: perp, side, tex: t, texX };
}

function losClear(x0, y0, x1, y1) {
    const dx = x1 - x0, dy = y1 - y0;
    const d = Math.sqrt(dx * dx + dy * dy);
    const steps = Math.max(1, Math.ceil(d / 0.1));
    for (let i = 1; i < steps; i++) {
        if (solid(Math.floor(x0 + dx * (i / steps)), Math.floor(y0 + dy * (i / steps)))) return false;
    }
    return true;
}

// ---- actors ---------------------------------------------------------------

const TYPES = [
    { name: "Imp",      world: 0.72, hp: 120, dmg: 8,  speed: 1.8 },
    { name: "Zombie",   world: 0.66, hp: 60,  dmg: 6,  speed: 1.3 },
    { name: "Demon",    world: 0.9,  hp: 220, dmg: 11, speed: 2.4 },
];
const FRAMES = { Imp: 37, Zombie: 39, Demon: 70 };
const texCache = {};
let sprites = [];
let px = 16.5, py = 9.5, yaw = Math.PI;
let hp = 100, kills = 0, dead = false;
let flash = 0, hurt = 0, muzzle = 0;
let fireCd = 0, painCd = 0;
let shotVol = 0.5, sndPistol = -1, sndPain = -1, sndDeathP = -1;

function frameTexture(kind, i) {
    const key = kind + "_" + i;
    let t = texCache[key];
    if (t === undefined) {
        t = loadTexture("Frames/" + kind + "_" + ("0" + i).slice(-2) + ".png");
        texCache[key] = t;
    }
    return t;
}

function actorSounds(kind) {
    const files = {
        Imp: ["imp sight 1.wav", "imp sight 2.wav", "imp death 1.wav", ""],
        Zombie: ["possessed sight 1.wav", "possessed sight 2.wav", "possessed death 1.wav", "possessed pain.wav"],
        Demon: ["pinky sight.wav", "", "pinky death.wav", ""],
    }[kind];
    return files.map((f) => (f ? loadSound("SFX16/" + f) : -1));
}
const soundCache = {};

function resetWorld() {
    generateLevel();
    sprites.length = 0;
    const spots = [
        [4.5, 3.5, 0], [15.5, 3.5, 1], [24.5, 4.5, 0], [28.5, 3.5, 2],
        [4.5, 15.5, 0], [15.5, 15.5, 2], [25.5, 15.5, 1], [8.5, 9.5, 0], [23.5, 9.5, 0],
    ];
    for (const s of spots) {
        const t = TYPES[s[2]];
        sprites.push({ x: s[0], y: s[1], type: s[2], hp: t.hp, anim: Math.random() * 100 | 0, sight: false, pain: 0 });
    }
    px = 16.5; py = 9.5;
    yaw = Math.PI;
}

// ---- movement / combat ----------------------------------------------------

const PW = 0.32;
function blocked(x, y) {
    return solid(Math.floor(x - PW), Math.floor(y - PW)) ||
        solid(Math.floor(x + PW), Math.floor(y - PW)) ||
        solid(Math.floor(x - PW), Math.floor(y + PW)) ||
        solid(Math.floor(x + PW), Math.floor(y + PW));
}
function stepPlayer(dt) {
    let fw = 0, st = 0;
    if (rl.isKeyDown(rl.KEY_W)) fw += 1;
    if (rl.isKeyDown(rl.KEY_S)) fw -= 1;
    if (rl.isKeyDown(rl.KEY_D)) st += 1;
    if (rl.isKeyDown(rl.KEY_A)) st -= 1;
    const speed = (rl.isKeyDown(rl.KEY_LEFT_SHIFT) ? 6.6 : 4.4) * dt;
    const dirX = Math.sin(yaw), dirY = Math.cos(yaw);
    const rx = -Math.cos(yaw), ry = Math.sin(yaw);
    let mx = dirX * fw + rx * st, my = dirY * fw + ry * st;
    const len = Math.sqrt(mx * mx + my * my);
    if (len > 0) {
        mx = mx / len * speed; my = my / len * speed;
        if (!blocked(px + mx, py)) px += mx;
        if (!blocked(px, py + my)) py += my;
    }
    yaw -= rl.getMouseDeltaX() * 0.0045;
}

function shoot() {
    const dirX = Math.sin(yaw), dirY = Math.cos(yaw);
    const rx = -Math.cos(yaw), ry = Math.sin(yaw);
    const wall = castRay(px, py, dirX, dirY);
    let best = null, bestDist = wall.dist;
    for (const s of sprites) {
        const relX = s.x - px, relY = s.y - py;
        const along = relX * dirX + relY * dirY;
        const across = relX * rx + relY * ry;
        if (along > 0 && along < bestDist && Math.abs(across) < 0.6) {
            best = s;
            bestDist = along;
        }
    }
    muzzle = 0.09;
    fireCd = 0.24;
    if (sndPistol >= 0) rl.playSound(sndPistol);
    if (best) {
        const dmg = 30 + (Math.random() * 16 | 0);
        best.hp -= dmg;
        best.pain = 0.3;
        flash = 0.08;
        const sc = soundCache[TYPES[best.type].name];
        if (sc && sc[3] >= 0) rl.playSound(sc[3]);
        if (best.hp <= 0) {
            best.hp = 0;
            kills += 1;
            const sc = soundCache[TYPES[best.type].name];
            if (sc && sc[2] >= 0) rl.playSound(sc[2]);
        }
    }
}

// ---- rendering ------------------------------------------------------------

const zBuf = new Float32Array(SCREEN_W);
let frame = 0;

function render() {
    const dirX = Math.sin(yaw), dirY = Math.cos(yaw);
    const planeX = -Math.cos(yaw) * 0.66, planeY = Math.sin(yaw) * 0.66;

    rl.clearBackground(rgba(14, 15, 20, 255));
    const horizon = SCREEN_H >> 1;
    for (let i = 0; i < 12; i++) {
        const y0 = horizon + (i * (SCREEN_H - horizon)) / 12;
        const y1 = horizon + ((i + 1) * (SCREEN_H - horizon)) / 12;
        const f = 0.28 + (i / 12) * 0.5;
        rl.drawRectangle(0, y0 | 0, SCREEN_W, Math.max(1, (y1 - y0) | 0),
            rgba(58 + f * 60 | 0, 50 + f * 50 | 0, 46 + f * 44 | 0, 255));
    }

    for (let col = 0; col < SCREEN_W; col++) {
        const camX = (2 * col) / SCREEN_W - 1;
        const wall = castRay(px, py, dirX + planeX * camX, dirY + planeY * camX);
        const d = Math.max(wall.dist, 0.001);
        const lineH = SCREEN_H / d;
        const top = (SCREEN_H - lineH) / 2;
        zBuf[col] = d;
        const shade = Math.min(1, 1.3 / (1 + d * 0.2)) * (wall.side === 0 ? 1 : 0.7);
        rl.drawTexture(wallTex[wall.tex], wall.texX, 0, 1, TEX_W, col, top, 1, lineH, shade);
    }

    // Actors far to near; animated through their ripped frame sheets.
    const vis = [];
    for (const s of sprites) {
        if (s.hp <= 0) continue;
        const relX = s.x - px, relY = s.y - py;
        const invDet = 1 / (planeX * dirY - dirX * planeY);
        const tX = invDet * (dirY * relX - dirX * relY);
        const tY = invDet * (-planeY * relX + planeX * relY);
        if (tY > 0.05) vis.push({ s, tX, tY });
    }
    vis.sort((a, b) => b.tY - a.tY);
    for (const v of vis) {
        const s = v.s;
        const kind = TYPES[s.type].name;
        const n = FRAMES[kind];
        let tex;
        if (s.hp <= 0) {
            tex = frameTexture(kind, 0); // freeze on a frame when dead
        } else {
            const idx = Math.floor((frame + s.anim) / 8) % n;
            tex = frameTexture(kind, idx);
        }
        if (tex < 0) continue;
        const tw = rl.textureWidth(tex), th = rl.textureHeight(tex);
        const worldH = TYPES[s.type].world;
        const spriteH = (SCREEN_H / v.tY) * worldH;
        const spriteW = spriteH * (tw / th);
        const centerX = (SCREEN_W / 2) * (1 + v.tX / v.tY);
        const left = centerX - spriteW / 2;
        const top = (SCREEN_H - spriteH) / 2 + Math.sin((frame + s.anim) * 0.35) * 2;
        const c = Math.max(0, Math.min(SCREEN_W - 1, centerX | 0));
        if (v.tY >= zBuf[c]) continue;
        const shade = Math.min(1, 1.3 / (1 + v.tY * 0.2));
        rl.drawTexture(tex, 0, 0, tw, th, left, top, spriteW, spriteH, shade);
        if (s.pain > 0) {
            rl.drawTexture(tex, 0, 0, tw, th, left, top, spriteW, spriteH, shade * 0.5);
        }
    }
}

function drawHud() {
    if (muzzle > 0) rl.drawCircle(SCREEN_W / 2, SCREEN_H - 60, 24, rgba(255, 220, 130, 150));
    if (flash > 0) rl.drawRectangle(0, 0, SCREEN_W, SCREEN_H, rgba(255, 255, 255, 110));
    if (hurt > 0) rl.drawRectangle(0, 0, SCREEN_W, SCREEN_H, rgba(255, 30, 20, 80));
    rl.drawRectangle(SCREEN_W / 2 - 5, SCREEN_H / 2 - 1, 10, 2, rgba(255, 255, 255, 230));
    rl.drawRectangle(SCREEN_W / 2 - 1, SCREEN_H / 2 - 5, 2, 10, rgba(255, 255, 255, 230));
    rl.drawRectangle(0, SCREEN_H - 40, SCREEN_W, 40, rgba(24, 22, 26, 255));
    rl.drawText("HP " + Math.max(0, hp), 12, SCREEN_H - 32, 18, rgba(255, 90, 80, 255));
    rl.drawText("KILLS " + kills, SCREEN_W - 240, SCREEN_H - 32, 18, rgba(255, 220, 130, 255));
    if (dead) {
        rl.drawText("YOU DIED - press R to restart", SCREEN_W / 2 - 185, SCREEN_H / 2 - 30, 30, rgba(255, 40, 30, 255));
    } else {
        rl.drawText("WASD move | mouse look | click fire | R restart", 10, 8, 14, rgba(190, 190, 195, 255));
    }
    rl.drawFPS(SCREEN_W - 76, 6);
}

// ---- main -----------------------------------------------------------------

function run() {
    rl.initWindow(SCREEN_W, SCREEN_H, "DOOM-ish (real sprites + sfx) in Slag + raylib");
    rl.setTargetFPS(60);
    rl.disableCursor();
    rl.initAudioDevice();

    buildWallTextures();
    sndPistol = loadSound("SFX16/pistol.wav");
    sndPain = loadSound("SFX16/player pain.wav");
    soundCache.Imp = actorSounds("Imp");
    soundCache.Zombie = actorSounds("Zombie");
    soundCache.Demon = actorSounds("Demon");
    for (const k in soundCache) {
        for (const id of soundCache[k]) if (id >= 0) rl.setSoundVolume(id, 0.4);
    }
    if (sndPistol >= 0) rl.setSoundVolume(sndPistol, 0.55);
    if (sndPain >= 0) rl.setSoundVolume(sndPain, 0.5);

    resetWorld();
    hp = 100; kills = 0; dead = false;

    while (!rl.windowShouldClose()) {
        const dt = Math.min(rl.getFrameTime(), 0.05);
        frame += 1;
        if (rl.isKeyPressed(rl.KEY_R) && (dead || true)) {
            resetWorld();
            hp = 100; kills = 0; dead = false;
        }
        if (!dead) stepPlayer(dt);

        fireCd -= dt; muzzle -= dt; flash -= dt; hurt -= dt; painCd -= dt;
        if (!dead && rl.isMouseButtonPressed(rl.MOUSE_BUTTON_LEFT) && fireCd <= 0) shoot();

        for (const s of sprites) {
            if (s.hp <= 0) continue;
            const dx = px - s.x, dy = py - s.y;
            const d = Math.sqrt(dx * dx + dy * dy);
            if (d < 10 && losClear(s.x, s.y, px, py)) {
                if (!s.sight) {
                    s.sight = true;
                    const sc = soundCache[TYPES[s.type].name];
                    if (sc && sc[0] >= 0) rl.playSound(sc[0]);
                }
                if (d > 0.9) {
                    const step = Math.min(TYPES[s.type].speed * dt, d - 0.9);
                    const nx = s.x + (dx / d) * step;
                    const ny = s.y + (dy / d) * step;
                    if (!solid(Math.floor(nx), Math.floor(ny))) { s.x = nx; s.y = ny; }
                    else if (!solid(Math.floor(nx), Math.floor(s.y))) s.x = nx;
                    else if (!solid(Math.floor(s.x), Math.floor(ny))) s.y = ny;
                }
                if (d < 1.1 && !dead && painCd <= 0) {
                    painCd = 0.4;
                    hurt = 0.35;
                    hp -= TYPES[s.type].dmg + (Math.random() * 6 | 0);
                    if (sndPain >= 0) rl.playSound(sndPain);
                    if (hp <= 0) { hp = 0; dead = true; }
                }
            } else {
                s.sight = false;
            }
            if (s.pain > 0) s.pain -= dt;
        }

        rl.beginDrawing();
        render();
        drawHud();
        rl.endDrawing();

        if (frame % 600 === 0) {
            console.log("frame " + frame + " fps " + rl.getFPS() + " kills " + kills + " hp " + hp);
        }
    }
    rl.enableCursor();
    rl.closeAudioDevice();
    rl.closeWindow();
    console.log("window closed after " + frame + " frames");
}

run();
