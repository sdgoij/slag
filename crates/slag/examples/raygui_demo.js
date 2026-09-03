// The Slag × raygui demo: a small settings-driven canvas written entirely in
// JavaScript. Dots drift around a background you can recolor; every setting
// on the right is a raygui control drawn through the `rl.gui*` host module,
// inside the same immediate-mode loop as the rest of the scene. This is the
// stateful-control pattern from the module docs: a control that owns state
// takes the current value and returns `{ action, value }`, so the script
// feeds `value` back in on the next frame.

const WIDTH = 960;
const HEIGHT = 600;
const PANEL = 620; // left edge of the controls column; dots live left of it
const MAX_X = PANEL - 14; // dot bounce/wrap bounds (leave room for the panel)
const MAX_Y = HEIGHT - 30; // ...and for the status bar
const X = PANEL + 16; // controls column x
const CW = WIDTH - X - 16; // controls column width
const ROW = 34; // vertical stride between controls

let dots = 60; // dot count (slider, 2..200)
let dotSize = 6; // dot radius (slider, 2..14)
let dotSpeed = 150; // base speed px/s (slider, 0..400)
let bouncing = true; // checkbox: bounce off walls instead of wrapping
let rainbow = false; // toggle: cycle dot colors through the hue wheel
let shape = 0; // dropdown: 0 = circle, 1 = square
let shapeOpen = false; // the dropdown's own open/close state
let name = "raygui"; // text box contents
let editName = false; // whether the text box is being edited
let background = rl.color(28, 30, 42, 255); // color picker
let showAbout = false; // the About window box
let askReset = false; // the message box

// A cheap 0..1-hue → packed-color helper (no HSV↔RGB in the `rl` surface).
function rainbowColor(hue) {
    const r = Math.floor(127 + 127 * Math.sin(Math.PI * 2 * hue));
    const g = Math.floor(127 + 127 * Math.sin(Math.PI * 2 * (hue + 1 / 3)));
    const b = Math.floor(127 + 127 * Math.sin(Math.PI * 2 * (hue + 2 / 3)));
    return rl.color(r, g, b, 255);
}

// Each dot carries a per-dot speed factor and base hue so the field looks
// organic even when the sliders are static.
const flock = [];
function refill() {
    while (flock.length > dots) flock.pop();
    while (flock.length < dots) {
        const angle = Math.random() * Math.PI * 2;
        flock.push({
            x: dotSize + Math.random() * (MAX_X - 2 * dotSize),
            y: dotSize + Math.random() * (MAX_Y - 2 * dotSize),
            vx: Math.cos(angle),
            vy: Math.sin(angle),
            speed: 0.5 + Math.random() * 0.9,
            hue: Math.random(),
        });
    }
}
refill();

function resetDefaults() {
    dots = 60;
    dotSize = 6;
    dotSpeed = 150;
    bouncing = true;
    rainbow = false;
    shape = 0;
    name = "raygui";
    background = rl.color(28, 30, 42, 255);
    refill();
}

rl.initWindow(WIDTH, HEIGHT, "Slag runs raygui");
rl.setTargetFPS(60);

let frame = 0;
while (!rl.windowShouldClose()) {
    const dt = Math.min(rl.getFrameTime(), 0.05);
    frame += 1;
    const t = frame / 60;

    // ---- update the dots ----
    for (const dot of flock) {
        const step = dot.speed * dotSpeed * dt;
        dot.x += dot.vx * step;
        dot.y += dot.vy * step;
        if (bouncing) {
            if (dot.x < dotSize) { dot.x = dotSize; dot.vx = Math.abs(dot.vx); }
            if (dot.x > MAX_X - dotSize) { dot.x = MAX_X - dotSize; dot.vx = -Math.abs(dot.vx); }
            if (dot.y < dotSize) { dot.y = dotSize; dot.vy = Math.abs(dot.vy); }
            if (dot.y > MAX_Y - dotSize) { dot.y = MAX_Y - dotSize; dot.vy = -Math.abs(dot.vy); }
        } else {
            if (dot.x < -dotSize) dot.x = MAX_X + dotSize;
            if (dot.x > MAX_X + dotSize) dot.x = -dotSize;
            if (dot.y < -dotSize) dot.y = MAX_Y + dotSize;
            if (dot.y > MAX_Y + dotSize) dot.y = -dotSize;
        }
    }

    rl.beginDrawing();
    rl.clearBackground(rl.BLACK);

    // ---- the canvas: background, title, then the dots ----
    rl.drawRectangle(0, 0, PANEL - 8, HEIGHT, background);
    rl.drawText(name + " — " + flock.length + " dots at " + dotSpeed + "px/s", 16, 14, 20, rl.RAYWHITE);
    for (const dot of flock) {
        const color = rainbow ? rainbowColor((t * 0.25 + dot.hue) % 1) : rainbowColor(dot.hue);
        if (shape === 0) {
            rl.drawCircle(dot.x | 0, dot.y | 0, dotSize, color);
        } else {
            const s = dotSize | 0;
            rl.drawRectangle((dot.x - s) | 0, (dot.y - s) | 0, s * 2, s * 2, color);
        }
    }

    // ---- the settings column (drawn over the canvas area it occupies) ----
    const picked = rl.guiColorPicker(X, 12, 180, 180, "", background);
    background = picked.value;

    let y = 200;
    rl.guiLabel(X, y, CW, 20, "Scene settings (raygui controls)");
    y += ROW;

    dots = Math.round(rl.guiSlider(X, y, CW, 24, "count", "", dots, 2, 200).value);
    y += ROW;
    dotSize = Math.round(rl.guiSlider(X, y, CW, 24, "size", "", dotSize, 2, 14).value);
    y += ROW;
    dotSpeed = rl.guiSlider(X, y, CW, 24, "speed", "", dotSpeed, 0, 400).value;
    y += ROW;

    const bounce = rl.guiCheckBox(X, y, CW, 24, "bounce off the walls", bouncing);
    bouncing = bounce.value;
    y += ROW;

    const toggle = rl.guiToggle(X, y, CW, 24, "cycle colors (rainbow)", rainbow);
    rainbow = toggle.value;
    y += ROW;

    // The dropdown reports both opening/closing and picking an item as
    // `action`; the script mirrors raygui's own idiom of toggling its
    // `editMode` on any action, then adopts the returned index.
    const drop = rl.guiDropdownBox(X, y, CW, 24, "Circle;Square", shape, shapeOpen);
    if (drop.action) shapeOpen = !shapeOpen;
    shape = drop.value;
    y += ROW + 6;

    rl.guiLabel(X, y, CW, 20, "Text (click to edit, Enter to commit)");
    y += ROW - 12;
    const box = rl.guiTextBox(X, y, CW, 24, name, editName);
    name = box.value;
    if (box.action) editName = !editName;
    y += ROW + 6;

    // A display-only pulse: the value is recomputed every frame, not stored.
    const pulse = (1 + Math.sin(t * 2)) / 2;
    rl.guiProgressBar(X, y, CW, 24, "pulse", "", pulse, 0, 1);
    y += ROW + 4;

    if (rl.guiButton(X, y, (CW - 12) / 2, 28, "About")) showAbout = true;
    if (rl.guiButton(X + (CW + 12) / 2, y, (CW - 12) / 2, 28, "Reset")) askReset = true;

    // ---- popups last, so they sit on top of everything ----
    // The About window box; guiWindowBox returns true only when its close
    // button was pressed, which hides it.
    if (showAbout) {
        if (rl.guiWindowBox(210, 130, 540, 220, "Slag runs raygui")) showAbout = false;
        rl.guiLabel(240, 190, 480, 24, "Bouncing dots driven by raygui controls,");
        rl.guiLabel(240, 216, 480, 24, "scripted in Slag's JavaScript.");
        rl.guiLabel(240, 242, 480, 24, "Drag sliders, type in the box, recolor the canvas.");
        if (rl.guiButton(600, 300, 110, 28, "OK")) showAbout = false;
    }

    // The message box returns the 1-based index of the clicked button (0 if
    // closed, -1 while open), which the docs spell out per control.
    if (askReset) {
        const answer = rl.guiMessageBox(220, 180, 520, 150, "Reset settings?", "Put the demo back to its defaults?", "YES;NO");
        if (answer > 0) {
            if (answer === 1) resetDefaults();
            askReset = false;
        }
    }

    rl.guiStatusBar(0, HEIGHT - 24, WIDTH, 24, "fps " + rl.getFPS() + "  •  shape: " + (shape === 0 ? "circle" : "square") + "  •  text: " + name);
    rl.endDrawing();
}

console.log("window closed after " + frame + " frames");
rl.closeWindow();
