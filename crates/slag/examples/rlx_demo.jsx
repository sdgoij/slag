// The Slag × rlx demo, authored in JSX. JSX elements desugar to rlx.h(...)
// calls at parse time, so the tree the reconciler sees is identical to an
// h()-built one. The parser gate is opt-in: the host evaluates this file
// with Context::eval_jsx.
//
// Text children are raw JSXText (scanned, then Babel-cleaned); a capitalized
// element like <Column> is a component reference, lowercase names are
// intrinsics.

const WIDTH = 960;
const HEIGHT = 560;
const PANEL = 640; // canvas lives left of here, controls right
const CANVAS_W = PANEL - 30; // ball bounce bounds
const CANVAS_H = HEIGHT - 56;
const X = PANEL + 16; // controls column x
const CW = WIDTH - X - 32; // controls column width

function rainbowColor(hue) {
    const r = Math.floor(127 + 127 * Math.sin(Math.PI * 2 * hue));
    const g = Math.floor(127 + 127 * Math.sin(Math.PI * 2 * (hue + 1 / 3)));
    const b = Math.floor(127 + 127 * Math.sin(Math.PI * 2 * (hue + 2 / 3)));
    return rl.color(r, g, b, 255);
}

// Layout component: stack element children down the column, inheriting
// x/width and giving each its declared height plus a gap. It lays children
// out programmatically, so it still builds rows with rlx.h internally.
function Column(props) {
    const gap = props.gap !== undefined ? props.gap : 14;
    const children = props.children === undefined ? [] :
        (Array.isArray(props.children) ? props.children : [props.children]);
    const rows = [];
    let y = props.y;
    for (let i = 0; i < children.length; i++) {
        const item = children[i];
        const height = item.props.height !== undefined ? item.props.height : 24;
        rows.push(rlx.h(item.type, Object.assign({}, item.props, {
            x: props.x,
            y: y,
            width: props.width,
            height: height,
        }), item.children));
        y += height + gap;
    }
    return rows;
}

// The root component: state + physics + the whole frame's JSX tree.
function Demo(props) {
    const speed = rlx.useState(180);
    const radius = rlx.useState(10);
    const rainbow = rlx.useState(true);
    const paused = rlx.useState(false);
    const caption = rlx.useState("bounce!");
    const editing = rlx.useState(false);
    const ball = rlx.useRef({ x: 260, y: 200, vx: 0.9, vy: 0.55 });

    // Advance the ball (the ref survives across frames).
    if (!paused[0]) {
        const b = ball.current;
        const step = speed[0] * props.dt;
        b.x += b.vx * step;
        b.y += b.vy * step;
        const r = radius[0];
        if (b.x < r) { b.x = r; b.vx = Math.abs(b.vx); }
        if (b.x > CANVAS_W - r) { b.x = CANVAS_W - r; b.vx = -Math.abs(b.vx); }
        if (b.y < r) { b.y = r; b.vy = Math.abs(b.vy); }
        if (b.y > CANVAS_H - r) { b.y = CANVAS_H - r; b.vy = -Math.abs(b.vy); }
    }

    const color = rainbow[0] ? rainbowColor((props.t * 0.3) % 1) : rl.GOLD;

    function resetAll() {
        speed[1](180);
        radius[1](10);
        rainbow[1](true);
        paused[1](false);
        caption[1]("bounce!");
        editing[1](false);
        const b = ball.current;
        b.x = 260;
        b.y = 200;
        b.vx = 0.9;
        b.vy = 0.55;
    }

    return [
        <canvas x={0} y={0} width={PANEL - 8} height={HEIGHT} color={rl.color(24, 26, 38, 255)} />,
        <ball x={ball.current.x} y={ball.current.y} radius={radius[0]} color={color} />,
        <caption x={16} y={14} size={20} color={rl.RAYWHITE}>{caption[0] + " — drag, toggle, type"}</caption>,

        <panel x={PANEL - 8} y={8} width={WIDTH - PANEL + 8} height={HEIGHT - 40} />,
        <Column x={X} y={32} width={CW} gap={12}>
            <label height={18}>Settings (controlled)</label>
            <slider height={24} value={speed[0]} min={0} max={400}
                left={String(Math.round(speed[0]))} right={"400"}
                onChange={function (event) { speed[1](event.value); }} />
            <slider height={24} value={radius[0]} min={4} max={40}
                left={String(radius[0])} right={"40"}
                onChange={function (event) { radius[1](Math.round(event.value)); }} />
            <checkbox height={24} checked={rainbow[0]}
                onChange={function (event) { rainbow[1](event.value); }}>rainbow ball</checkbox>
            <toggle height={24} active={paused[0]}
                onChange={function (event) { paused[1](event.value); }}>pause</toggle>
            <label height={16}>caption (click to edit, Enter to commit)</label>
            <textbox height={24} value={caption[0]} edit={editing[0]}
                onChange={function (event) {
                    caption[1](event.value);
                    if (event.action) editing[1](function (prev) { return !prev; });
                }} />
            <button height={28} onClick={resetAll}>Reset</button>
        </Column>,
        <status x={0} y={HEIGHT - 24} width={WIDTH} height={24}>
            {"fps " + rl.getFPS() + "  •  ball at " + Math.round(ball.current.x) + "," +
                Math.round(ball.current.y) + "  •  " + caption[0]}
        </status>,
    ];
}

// Wrap the stock backend with the demo's canvas ops.
const stockDraw = rlx.draw;
rlx.draw = function (op) {
    switch (op.type) {
        case "canvas":
            rl.drawRectangle(op.props.x | 0, op.props.y | 0, op.props.width | 0, op.props.height | 0, op.props.color);
            return undefined;
        case "ball":
            rl.drawCircle(Math.round(op.props.x), Math.round(op.props.y), op.props.radius, op.props.color);
            return undefined;
        case "caption":
            rl.drawText(op.text, op.props.x | 0, op.props.y | 0, op.props.size | 0, op.props.color);
            return undefined;
        default:
            return stockDraw(op);
    }
};

rl.initWindow(WIDTH, HEIGHT, "rlx declarative layer (JSX)");
rl.setTargetFPS(60);

let frame = 0;
while (!rl.windowShouldClose()) {
    frame += 1;
    const dt = Math.min(rl.getFrameTime(), 0.05);
    const t = frame / 60;

    rl.beginDrawing();
    rl.clearBackground(rl.BLACK);
    rlx.present(<Demo dt={dt} t={t} />);
    rl.endDrawing();
}

console.log("window closed after " + frame + " frames");
rl.closeWindow();
