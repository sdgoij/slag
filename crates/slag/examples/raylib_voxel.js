// A tiny "Minecraft-like" voxel sandbox in pure JavaScript, rendered through
// the `rl` host module's 3D surface.
//
// Controls:
//   mouse         look around (arrows work too)   WASD   move
//   SPACE         jump                             ESC    quit
//   left click    remove block     right click     place block
//   keys 1..6     select block to place (grass/dirt/stone/sand/wood/leaves)

const W = 26;          // world is W x W columns
const H = 16;          // world height (y up, y = 0..H-1)
const TILE = 1.0;      // one block == one world unit

// Block ids.
const AIR = 0, GRASS = 1, DIRT = 2, STONE = 3, SAND = 4, WOOD = 5, LEAVES = 6;

// Packed 0xRRGGBBAA colors (same convention as rl.color).
function col(r, g, b) {
    return ((r * 256 + g) * 256 + b) * 256 + 255;
}

const BLOCK_COLOR = [];
BLOCK_COLOR[GRASS] = col(89, 178, 64);
BLOCK_COLOR[DIRT] = col(150, 107, 75);
BLOCK_COLOR[STONE] = col(140, 140, 143);
BLOCK_COLOR[SAND] = col(235, 222, 166);
BLOCK_COLOR[WOOD] = col(115, 82, 51);
BLOCK_COLOR[LEAVES] = col(45, 120, 45);
const BLOCK_NAME = [];
BLOCK_NAME[GRASS] = "grass";
BLOCK_NAME[DIRT] = "dirt";
BLOCK_NAME[STONE] = "stone";
BLOCK_NAME[SAND] = "sand";
BLOCK_NAME[WOOD] = "wood";
BLOCK_NAME[LEAVES] = "leaves";
const PLACEABLE = [GRASS, DIRT, STONE, SAND, WOOD, LEAVES];

// Deterministic hashes (the world is identical every run).
function hash2(x, z) {
    const n = Math.sin(x * 12.9898 + z * 78.233) * 43758.5453;
    return n - Math.floor(n);
}

function heightAt(x, z) {
    const roll = Math.sin(x * 0.6 + 0.3) * Math.cos(z * 0.5 + 0.8);
    const bump = Math.sin(x * 1.3 + 2.0) * Math.cos(z * 1.1 + 0.4);
    const jitter = hash2(x, z) - 0.5;
    let h = 2.6 + roll * 2.0 + bump * 1.3 + jitter * 0.9;
    if (h < 1.4) h = 1.4;
    if (h > 6.2) h = 6.2;
    return Math.round(h);
}

// ---- voxel storage -------------------------------------------------------

const world = new Uint8Array(H * W * W);
function idx(x, y, z) {
    return (y * W + z) * W + x;
}
function inWorld(x, y, z) {
    return x >= 0 && x < W && z >= 0 && z < W && y >= 0 && y < H;
}
function solidAt(x, y, z) {
    return inWorld(x, y, z) && world[idx(x, y, z)] !== AIR;
}
function blockAt(x, y, z) {
    return inWorld(x, y, z) ? world[idx(x, y, z)] : AIR;
}

function generateWorld() {
    for (let x = 0; x < W; x++) {
        for (let z = 0; z < W; z++) {
            const h = heightAt(x, z);
            const sandy = h <= 2;
            const high = h >= 5;
            for (let y = 0; y < h; y++) {
                let t;
                if (y === 0) t = STONE;                       // unbreakable floor
                else if (y === h - 1) t = sandy ? SAND : high ? STONE : GRASS;
                else if (y >= h - 3) t = sandy ? SAND : DIRT;
                else t = STONE;
                world[idx(x, y, z)] = t;
            }
        }
    }
    // Scatter a few trees on grass tops.
    for (let x = 2; x < W - 2; x++) {
        for (let z = 2; z < W - 2; z++) {
            const h = heightAt(x, z);
            if (h < 2 || h > 4) continue;
            if (blockAt(x, h - 1, z) !== GRASS) continue;
            if (hash2(x * 3 + 11, z * 5 + 7) > 0.035) continue;
            const trunk = 3 + Math.floor(hash2(x + 9, z + 4) * 2);
            const top = h + trunk;
            for (let y = h; y < top; y++) world[idx(x, y, z)] = WOOD;
            // Cross of leaves, then a 3x3 cap.
            for (let dx = -1; dx <= 1; dx++) {
                for (let dz = -1; dz <= 1; dz++) {
                    if (dx !== 0 && dz !== 0) continue;
                    if (inWorld(x + dx, top, z + dz)) world[idx(x + dx, top, z + dz)] = LEAVES;
                }
            }
            for (let dx = -1; dx <= 1; dx++) {
                for (let dz = -1; dz <= 1; dz++) {
                    if (inWorld(x + dx, top + 1, z + dz)) world[idx(x + dx, top + 1, z + dz)] = LEAVES;
                }
            }
        }
    }
}

// Blocks with any air face; rebuilt only when the world changes.
const cubes = [];
function rebuildCubes() {
    cubes.length = 0;
    for (let y = 0; y < H; y++) {
        for (let z = 0; z < W; z++) {
            for (let x = 0; x < W; x++) {
                const t = world[idx(x, y, z)];
                if (t === AIR) continue;
                const hidden =
                    (y + 1 < H && world[idx(x, y + 1, z)] !== AIR) &&
                    (y > 0 ? world[idx(x, y - 1, z)] !== AIR : true) &&
                    (x + 1 < W && world[idx(x + 1, y, z)] !== AIR) &&
                    (x > 0 && world[idx(x - 1, y, z)] !== AIR) &&
                    (z + 1 < W && world[idx(x, y, z + 1)] !== AIR) &&
                    (z > 0 && world[idx(x, y, z - 1)] !== AIR);
                if (!hidden) cubes.push(x, y, z, t);
            }
        }
    }
}

// ---- player --------------------------------------------------------------

const P_W = 0.5, P_H = 1.8, EYE = 1.6;
const SPEED = 5.0, GRAV = 26, JUMP = 8.6, MAX_FALL = 30;
let px = 0, py = 0, pz = 0, vy = 0, grounded = false;
let yaw = 0.6, pitch = -0.12;

function playerHits(x, y, z) {
    const x0 = Math.floor(x - P_W / 2), x1 = Math.floor(x + P_W / 2 - 1e-4);
    const y0 = Math.floor(y), y1 = Math.floor(y + P_H - 1e-4);
    const z0 = Math.floor(z - P_W / 2), z1 = Math.floor(z + P_W / 2 - 1e-4);
    for (let bx = x0; bx <= x1; bx++) {
        for (let by = y0; by <= y1; by++) {
            for (let bz = z0; bz <= z1; bz++) {
                if (solidAt(bx, by, bz)) return true;
            }
        }
    }
    return false;
}

function spawn() {
    const cx = Math.floor(W / 2) + 1, cz = Math.floor(W / 2) + 1;
    const naturalTop = (x, z) => {
        let top = H - 1;
        while (top > 0 && !solidAt(x, top, z)) top -= 1;
        const t = blockAt(x, top, z);
        return t === GRASS || t === SAND || t === STONE ? top : -1;
    };
    // Spiral outward from the center until we find a natural ground column
    // (never a tree canopy).
    for (let r = 0; r <= 7; r++) {
        for (let dx = -r; dx <= r; dx++) {
            for (let dz = -r; dz <= r; dz++) {
                if (Math.max(Math.abs(dx), Math.abs(dz)) !== r) continue;
                const colx = cx + dx, colz = cz + dz;
                if (colx < 1 || colx >= W - 1 || colz < 1 || colz >= W - 1) continue;
                const top = naturalTop(colx, colz);
                if (top < 0) continue;
                const sx = colx + 0.5, sz = colz + 0.5;
                if (!playerHits(sx, top + 1, sz)) {
                    px = sx;
                    pz = sz;
                    py = top + 1;
                    vy = 0;
                    grounded = false;
                    return;
                }
            }
        }
    }
}

function stepPlayer(dt, mx, mz) {
    // Horizontal move, per axis, with one-block auto-step so gentle slopes
    // and single stairs are walkable without jumping.
    const nx = px + mx * SPEED * dt;
    if (!playerHits(nx, py, pz)) {
        px = nx;
    } else if (mx !== 0 && !playerHits(nx, py + 1, pz) && !playerHits(nx, py + 2, pz)) {
        px = nx;
        py += 1; // step up one block
    }
    const nz = pz + mz * SPEED * dt;
    if (!playerHits(px, py, nz)) {
        pz = nz;
    } else if (mz !== 0 && !playerHits(px, py + 1, nz) && !playerHits(px, py + 2, nz)) {
        pz = nz;
        py += 1;
    }
    px = Math.max(P_W / 2 + 0.01, Math.min(W - P_W / 2 - 0.01, px));
    pz = Math.max(P_W / 2 + 0.01, Math.min(W - P_W / 2 - 0.01, pz));

    // Gravity and vertical move.
    vy -= GRAV * dt;
    if (vy < -MAX_FALL) vy = -MAX_FALL;
    const ny = py + vy * dt;
    if (!playerHits(px, ny, pz)) {
        py = ny;
        grounded = false;
    } else if (vy < 0) {
        py = Math.floor(ny) + 1; // feet land on the top of the block below
        vy = 0;
        grounded = true;
    } else {
        py = Math.floor(ny + P_H - 1e-4) - P_H; // head hit the block above
        vy = 0;
    }
}

// ---- aim / interaction ---------------------------------------------------

const REACH = 5.5;

function raycast() {
    const ex = px, ey = py + EYE, ez = pz;
    const fx = Math.cos(pitch) * Math.sin(yaw);
    const fy = Math.sin(pitch);
    const fz = Math.cos(pitch) * Math.cos(yaw);
    let place = null;
    for (let d = 0.0; d <= REACH; d += 0.05) {
        const bx = Math.floor(ex + fx * d);
        const by = Math.floor(ey + fy * d);
        const bz = Math.floor(ez + fz * d);
        if (solidAt(bx, by, bz)) {
            return { hit: [bx, by, bz], place: place };
        }
        place = [bx, by, bz];
    }
    return null;
}

function breakBlock(bx, by, bz) {
    if (by <= 0) return; // keep the floor
    const t = blockAt(bx, by, bz);
    if (t === AIR) return;
    world[idx(bx, by, bz)] = AIR;
    rebuildCubes();
}

// If a placed block ends up intersecting the player, lift the player onto
// its top (Minecraft's "place a block under your feet" behavior). Returns
// false if there is no room after several pushes (caller reverts the block).
function pushPlayerUp() {
    for (let iter = 0; iter < 16; iter++) {
        if (!playerHits(px, py, pz)) return true;
        const x0 = Math.floor(px - P_W / 2), x1 = Math.floor(px + P_W / 2 - 1e-4);
        const y0 = Math.floor(py), y1 = Math.floor(py + P_H - 1e-4);
        const z0 = Math.floor(pz - P_W / 2), z1 = Math.floor(pz + P_W / 2 - 1e-4);
        let top = -1;
        for (let bx = x0; bx <= x1; bx++) {
            for (let by = y0; by <= y1; by++) {
                for (let bz = z0; bz <= z1; bz++) {
                    if (solidAt(bx, by, bz) && by > top) top = by;
                }
            }
        }
        if (top < 0) break;
        py = top + 1;
        vy = 0;
        grounded = true;
    }
    return !playerHits(px, py, pz);
}

function placeBlock(bx, by, bz, t) {
    if (t === AIR || by <= 0 || by >= H) return;
    if (solidAt(bx, by, bz)) return;
    world[idx(bx, by, bz)] = t;
    rebuildCubes();
    // If the new block overlaps the player (placed at their feet), push them
    // up onto it instead of trapping them; revert if there is no room.
    if (playerHits(px, py, pz) && !pushPlayerUp()) {
        world[idx(bx, by, bz)] = AIR;
        rebuildCubes();
    }
}

// ---- main ----------------------------------------------------------------

let selected = GRASS;
let frame = 0;
let lastLog = 0;

function run() {
    rl.initWindow(900, 600, "SlagCraft - a tiny Minecraft-like in Slag + raylib");
    rl.setTargetFPS(60);
    rl.disableCursor(); // mouse look: cursor hidden and locked

    generateWorld();
    rebuildCubes();
    spawn();
    console.log(
        "world ready: " + (cubes.length / 4) + " visible cubes, spawn " +
        px.toFixed(1) + "," + py.toFixed(1) + "," + pz.toFixed(1) + " on " + BLOCK_NAME[blockAt(Math.floor(px), Math.floor(py - 0.1), Math.floor(pz))],
    );

    const sky = col(108, 178, 236);

    while (!rl.windowShouldClose()) {
        const dt = Math.min(rl.getFrameTime(), 0.05);
        frame += 1;

        // Mouse look. Screen-right is cross(forward, up), which the camera
        // convention above implies is the mirror of the naive (cos, -sin)
        // right vector, so rightward mouse motion decreases yaw.
        const mdx = rl.getMouseDeltaX();
        const mdy = rl.getMouseDeltaY();
        yaw -= mdx * 0.0032;
        pitch -= mdy * 0.0032; // mouse up (dy < 0) looks up
        // Arrow-key fallback, matching the mouse signs.
        if (rl.isKeyDown(rl.KEY_LEFT)) yaw += 2.6 * dt;
        if (rl.isKeyDown(rl.KEY_RIGHT)) yaw -= 2.6 * dt;
        if (rl.isKeyDown(rl.KEY_UP)) pitch += 2.0 * dt;
        if (rl.isKeyDown(rl.KEY_DOWN)) pitch -= 2.0 * dt;
        if (pitch > 1.5) pitch = 1.5;
        if (pitch < -1.5) pitch = -1.5;

        // Move relative to where we are looking. Right = cross(forward, up)
        // = (-cos yaw, 0, sin yaw) for forward (sin yaw, 0, cos yaw).
        let fw = 0, st = 0;
        if (rl.isKeyDown(rl.KEY_W)) fw += 1;
        if (rl.isKeyDown(rl.KEY_S)) fw -= 1;
        if (rl.isKeyDown(rl.KEY_D)) st += 1;
        if (rl.isKeyDown(rl.KEY_A)) st -= 1;
        const fx = Math.sin(yaw), fz = Math.cos(yaw);
        const rx = -Math.cos(yaw), rz = Math.sin(yaw);
        let mx = fx * fw + rx * st;
        let mz = fz * fw + rz * st;
        const mlen = Math.sqrt(mx * mx + mz * mz);
        if (mlen > 0) { mx /= mlen; mz /= mlen; }

        if (rl.isKeyPressed(rl.KEY_SPACE) && grounded) vy = JUMP;
        stepPlayer(dt, mx, mz);
        if (py < -20) spawn();

        // Block type selection (1..6).
        const keyCodes = [rl.KEY_1, rl.KEY_2, rl.KEY_3, rl.KEY_4, rl.KEY_5, rl.KEY_6];
        for (let i = 0; i < PLACEABLE.length; i++) {
            if (rl.isKeyPressed(keyCodes[i])) selected = PLACEABLE[i];
        }

        // Target the block under the crosshair.
        const aim = raycast();
        let highlight = null;
        if (aim) {
            if (rl.isMouseButtonPressed(rl.MOUSE_BUTTON_LEFT)) {
                breakBlock(aim.hit[0], aim.hit[1], aim.hit[2]);
                aim.result = 1;
            } else if (rl.isMouseButtonPressed(rl.MOUSE_BUTTON_RIGHT) && aim.place) {
                placeBlock(aim.place[0], aim.place[1], aim.place[2], selected);
                aim.result = 2;
            }
            if (!aim.result) highlight = aim.hit;
        }

        // Render.
        const ex = px, ey = py + EYE, ez = pz;
        const lookX = Math.cos(pitch) * Math.sin(yaw);
        const lookY = Math.sin(pitch);
        const lookZ = Math.cos(pitch) * Math.cos(yaw);

        rl.beginDrawing();
        rl.clearBackground(sky);
        rl.beginMode3D(ex, ey, ez, ex + lookX, ey + lookY, ez + lookZ, 75);

        for (let i = 0; i < cubes.length; i += 4) {
            const cx = cubes[i], cy = cubes[i + 1], cz = cubes[i + 2];
            rl.drawCube(cx + 0.5, cy + 0.5, cz + 0.5, 1, 1, 1, BLOCK_COLOR[cubes[i + 3]]);
        }
        if (highlight) {
            rl.drawCubeWires(highlight[0] + 0.5, highlight[1] + 0.5, highlight[2] + 0.5, 1.01, 1.01, 1.01, rl.WHITE);
        }
        rl.endMode3D();

        // HUD.
        const fps = rl.getFPS();
        rl.drawText("SlagCraft - tiny Minecraft-like (Slag x raylib)", 10, 8, 18, rl.RAYWHITE);
        rl.drawText("mouse look | WASD move | SPACE jump | LMB remove | RMB place | 1-6 block | ESC quit", 10, 32, 14, rl.RAYWHITE);
        rl.drawText("placing: " + BLOCK_NAME[selected] + "   pos " + px.toFixed(1) + ", " + py.toFixed(1) + ", " + pz.toFixed(1) + "   cubes " + (cubes.length / 4) + "   fps " + fps, 10, 580, 14, rl.RAYWHITE);
        // Crosshair.
        rl.drawLine(450 - 8, 300, 450 - 3, 300, rl.WHITE);
        rl.drawLine(450 + 3, 300, 450 + 8, 300, rl.WHITE);
        rl.drawLine(450, 300 - 8, 450, 300 - 3, rl.WHITE);
        rl.drawLine(450, 300 + 3, 450, 300 + 8, rl.WHITE);

        rl.endDrawing();

        // Heartbeat so a scripted run can be watched.
        if (frame % 120 === 0) {
            console.log("frame " + frame + " cubes " + (cubes.length / 4) + " fps " + fps);
        }
    }
    console.log("window closed after " + frame + " frames");
    rl.enableCursor();
    rl.closeWindow();
}

// Headless self-test used by the harness (`slag raylib_voxel.js selftest`):
// place a block at the player's feet cell and check the player is pushed up
// onto it instead of getting stuck.
function selfTest() {
    generateWorld();
    rebuildCubes();
    spawn();
    for (let i = 0; i < 180; i++) stepPlayer(1 / 60, 0, 0); // settle under gravity
    if (!grounded) throw new Error("selftest: player never grounded");
    const restingY = py;
    const bx = Math.floor(px), bz = Math.floor(pz);
    const feetCell = Math.floor(py);
    if (solidAt(bx, feetCell, bz)) throw new Error("selftest: feet cell is not air");
    placeBlock(bx, feetCell, bz, GRASS);
    if (py <= restingY) throw new Error("selftest: player was not pushed up");
    if (playerHits(px, py, pz)) throw new Error("selftest: player stuck inside the placed block");
    if (!solidAt(bx, Math.floor(py) - 1, bz)) throw new Error("selftest: no block under feet after the push");
    console.log("selftest ok: resting y=" + restingY.toFixed(2) + " -> pushed to y=" + py.toFixed(2));
}

if (typeof process !== "undefined" && process.argv[2] === "selftest") {
    selfTest();
} else {
    run();
}
